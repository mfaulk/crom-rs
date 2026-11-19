//! Cedar policy decision point (ADR 0003 §4.4–§4.5). Cedar skips a policy
//! whose evaluation errors, so an erroring `forbid` stops forbidding; every
//! path here therefore treats any diagnostic or input error as a deny.

mod digest;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use cedar_policy as cedar;
use cedar_policy::RestrictedExpression as Expr;
use sha2::{Digest, Sha256};

pub(crate) use digest::args_digest_hex;
pub use digest::ACTION_DIGEST_DOMAIN;

use crate::action::AuthorizationRequest;
use crate::decision::{ActionDigest, ApprovalRequirement, Decision, DenyReason};
use crate::grant::{Delegator, Grant};
use crate::ids::{ActionId, AgentId, GrantId, PolicyId, TenantId, RESERVED_ID_PREFIX};
use crate::resource::ResolvedResource;
use crate::value::{AttrValue, Attrs, EntityUid};
use crate::ValidatedArgs;

/// Largest schema or policy source accepted, in bytes.
pub const MAX_POLICY_SOURCE_BYTES: usize = 1 << 20;

/// Reserved approver id used only inside the approval probe (ADR 0003 §4.4).
pub const APPROVAL_PLACEHOLDER_ID: &str = "__guard_approval_placeholder__";

/// Why a schema or policy set was refused. Guard must not start on any of
/// these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PdpLoadError {
    #[error("cannot read {path}: {message}")]
    Io { path: String, message: String },
    #[error("{what} source is larger than {MAX_POLICY_SOURCE_BYTES} bytes")]
    TooLarge { what: &'static str },
    #[error("namespace {0:?} is not a `::`-separated list of identifiers")]
    InvalidNamespace(String),
    #[error("schema does not parse: {0}")]
    SchemaParse(String),
    #[error("schema has warnings (treated as errors): {0:?}")]
    SchemaWarnings(Vec<String>),
    #[error("schema does not declare entity type {0}")]
    MissingEntityType(String),
    #[error("policy set does not parse: {0}")]
    PolicyParse(String),
    #[error("policy templates are not supported")]
    Templates,
    #[error("policy #{index} (0-based, in source order) has no @id annotation")]
    MissingId { index: usize },
    #[error("policy @id {0:?} is not a valid policy id")]
    InvalidId(String),
    #[error("policy @id {0:?} is used more than once")]
    DuplicateId(String),
    #[error("strict validation failed: {0:?}")]
    Validation(Vec<String>),
    #[error("policy {policy:?} has an invalid @approval annotation: {problem}")]
    Approval { policy: String, problem: String },
}

/// The outcome of evaluating one grant of the chain in the main evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEvaluation {
    pub grant_id: GrantId,
    pub allowed: bool,
    /// Sorted. For an allow, the satisfied permits; for a deny, the
    /// satisfied forbids (empty when no permit applied).
    pub determining_policies: Vec<PolicyId>,
    /// Sorted ids of policies whose evaluation errored.
    pub erroring_policies: Vec<PolicyId>,
}

/// Full result of [`CedarPdp::evaluate_chain`]. `decision` is what
/// [`authorize`](crate::PolicyDecisionPoint::authorize) returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub decision: Decision,
    /// Main evaluation per grant, leaf first, up to the first that denied
    /// or errored. Empty if the request was refused before evaluation.
    pub grants: Vec<GrantEvaluation>,
    /// Engine and input errors. May contain request values, so only the
    /// audit log (not the agent) may see them.
    pub errors: Vec<String>,
}

struct Types {
    agent: cedar::EntityTypeName,
    user: cedar::EntityTypeName,
    tenant: cedar::EntityTypeName,
    action: cedar::EntityTypeName,
}

/// Cedar-backed [`PolicyDecisionPoint`](crate::PolicyDecisionPoint). Built
/// once at startup from operator-supplied sources; there is no API to change
/// the policy set or schema afterwards.
///
/// Entity model v1 (ADR 0003 §4.2), all types in `namespace`:
/// `Agent::"<agent>"` {`tenant`, `delegator`}; `User::"<delegator>"`
/// {`tenant`, `role`, delegator attrs} (in its `Tenant` when the schema
/// allows); `Tenant::"<tenant>"`; the resolved resource with its attributes
/// and direct parents; for an approved request, the approver `User`, whose
/// tenant is the principal's. Entities the resource only references (its
/// parents, `resource.customer`) are not in the store, so a policy that
/// dereferences them errors and denies.
pub struct CedarPdp {
    namespace: String,
    schema: cedar::Schema,
    policies: cedar::PolicySet,
    /// Every forbid plus every `@approval` permit (ADR 0003 §4.4 step 3).
    probe_set: cedar::PolicySet,
    /// Distinct `@approval` values in policy source order.
    approval_roles: Vec<String>,
    /// Action ids the schema declares.
    actions: BTreeSet<String>,
    types: Types,
    user_in_tenant: bool,
    authorizer: cedar::Authorizer,
    policy_set_hash: String,
    schema_hash: String,
}

impl fmt::Debug for CedarPdp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CedarPdp")
            .field("namespace", &self.namespace)
            .field("policy_set_hash", &self.policy_set_hash)
            .field("schema_hash", &self.schema_hash)
            .field("approval_roles", &self.approval_roles)
            .field("actions", &self.actions)
            .finish_non_exhaustive()
    }
}

fn read_source(path: &Path) -> Result<String, PdpLoadError> {
    let io = |e: std::io::Error| PdpLoadError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    let mut s = String::new();
    std::fs::File::open(path)
        .map_err(io)?
        .take(MAX_POLICY_SOURCE_BYTES as u64 + 1)
        .read_to_string(&mut s)
        .map_err(io)?;
    Ok(s)
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn type_name(ns: &str, name: &str) -> Result<cedar::EntityTypeName, PdpLoadError> {
    cedar::EntityTypeName::from_str(&format!("{ns}::{name}"))
        .map_err(|_| PdpLoadError::InvalidNamespace(ns.to_owned()))
}

/// Checks the ADR 0003 §4.4 author requirements for an `@approval` permit.
fn check_approval(policy: &cedar::Policy, id: &str, role: &str) -> Result<(), PdpLoadError> {
    let fail = |problem: &str| PdpLoadError::Approval {
        policy: id.to_owned(),
        problem: problem.to_owned(),
    };
    if policy.effect() != cedar::Effect::Permit {
        return Err(fail("only permits may carry @approval"));
    }
    if role.is_empty()
        || !role
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(fail("the role must be non-empty [A-Za-z0-9_.-]"));
    }
    // Checked on the parsed policy (EST), not its text: both tests must be
    // top-level `&&` conjuncts of `when` clauses, so the permit cannot be
    // satisfied without an approval of that role. `unless` is refused
    // outright rather than reasoned about.
    let est = policy
        .to_json()
        .map_err(|e| fail(&format!("cannot inspect policy: {e}")))?;
    let mut conj = Vec::new();
    for c in est["conditions"].as_array().into_iter().flatten() {
        if c["kind"] != "when" {
            return Err(fail("an @approval permit may not have `unless` clauses"));
        }
        conjuncts(&c["body"], &mut conj);
    }
    let context = serde_json::json!({"Var": "context"});
    let has = serde_json::json!({"has": {"left": context, "attr": "approval"}});
    if !conj.contains(&&has) {
        return Err(fail(
            "a `when` clause must test `context has approval` as a top-level conjunct",
        ));
    }
    let path = ["approval", "approver", "role"].iter().fold(
        context,
        |l, a| serde_json::json!({".": {"left": l, "attr": a}}),
    );
    let value = serde_json::json!({"Value": role});
    let role_test = |l: &serde_json::Value, r: &serde_json::Value| serde_json::json!({"==": {"left": l, "right": r}});
    if !conj.contains(&&role_test(&path, &value)) && !conj.contains(&&role_test(&value, &path)) {
        return Err(fail(
            "a `when` clause must test `context.approval.approver.role == \"<role>\"` \
             as a top-level conjunct",
        ));
    }
    Ok(())
}

/// Flattens nested `&&` into its operands.
fn conjuncts<'a>(e: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
    match e.get("&&") {
        Some(and) => {
            conjuncts(&and["left"], out);
            conjuncts(&and["right"], out);
        }
        None => out.push(e),
    }
}

impl CedarPdp {
    /// Loads from operator paths. See [`from_sources`](Self::from_sources).
    pub fn from_files(
        namespace: &str,
        schema_path: &Path,
        policy_path: &Path,
    ) -> Result<Self, PdpLoadError> {
        let schema = read_source(schema_path)?;
        let policies = read_source(policy_path)?;
        Self::from_sources(namespace, &schema, &policies)
    }

    /// Parses the Cedar schema and policy set and validates them in strict
    /// mode, treating warnings as errors. Refuses templates, policies
    /// without a unique valid `@id`, and malformed `@approval` permits.
    /// Policies are re-keyed by `@id`. The hashes are hex SHA-256 of
    /// exactly these source strings.
    pub fn from_sources(
        namespace: &str,
        schema_src: &str,
        policy_src: &str,
    ) -> Result<Self, PdpLoadError> {
        if schema_src.len() > MAX_POLICY_SOURCE_BYTES {
            return Err(PdpLoadError::TooLarge { what: "schema" });
        }
        if policy_src.len() > MAX_POLICY_SOURCE_BYTES {
            return Err(PdpLoadError::TooLarge { what: "policy" });
        }
        if !namespace.split("::").all(is_ident) {
            return Err(PdpLoadError::InvalidNamespace(namespace.to_owned()));
        }
        let types = Types {
            agent: type_name(namespace, "Agent")?,
            user: type_name(namespace, "User")?,
            tenant: type_name(namespace, "Tenant")?,
            action: type_name(namespace, "Action")?,
        };

        let (schema, warnings) = cedar::Schema::from_cedarschema_str(schema_src)
            .map_err(|e| PdpLoadError::SchemaParse(e.to_string()))?;
        let warnings: Vec<String> = warnings.map(|w| w.to_string()).collect();
        if !warnings.is_empty() {
            return Err(PdpLoadError::SchemaWarnings(warnings));
        }
        for t in [&types.agent, &types.user, &types.tenant] {
            if !schema.entity_types().any(|s| s == t) {
                return Err(PdpLoadError::MissingEntityType(t.to_string()));
            }
        }

        let parsed = cedar::PolicySet::from_str(policy_src)
            .map_err(|e| PdpLoadError::PolicyParse(e.to_string()))?;
        if parsed.templates().next().is_some() {
            return Err(PdpLoadError::Templates);
        }
        let mut policies = cedar::PolicySet::new();
        let mut probe_set = cedar::PolicySet::new();
        let mut approval_roles: Vec<String> = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, p) in parsed.policies().enumerate() {
            let id = p
                .annotation("id")
                .ok_or(PdpLoadError::MissingId { index })?;
            PolicyId::new(id).map_err(|_| PdpLoadError::InvalidId(id.to_owned()))?;
            if !seen.insert(id.to_owned()) {
                return Err(PdpLoadError::DuplicateId(id.to_owned()));
            }
            let p = p.new_id(cedar::PolicyId::new(id));
            let approval = p.annotation("approval").map(str::to_owned);
            if let Some(role) = &approval {
                check_approval(&p, id, role)?;
                if !approval_roles.contains(role) {
                    approval_roles.push(role.clone());
                }
            }
            if p.effect() == cedar::Effect::Forbid || approval.is_some() {
                probe_set
                    .add(p.clone())
                    .map_err(|e| PdpLoadError::PolicyParse(e.to_string()))?;
            }
            policies
                .add(p)
                .map_err(|e| PdpLoadError::PolicyParse(e.to_string()))?;
        }

        let v = cedar::Validator::new(schema.clone())
            .validate(&policies, cedar::ValidationMode::Strict);
        let problems: Vec<String> = v
            .validation_errors()
            .map(|e| e.to_string())
            .chain(v.validation_warnings().map(|w| format!("warning: {w}")))
            .collect();
        if !problems.is_empty() {
            return Err(PdpLoadError::Validation(problems));
        }

        let actions = schema
            .actions()
            .filter(|a| a.type_name() == &types.action)
            .map(|a| a.id().unescaped().to_owned())
            .collect();
        let user_in_tenant = schema
            .ancestors(&types.user)
            .is_some_and(|mut a| a.any(|t| t == &types.tenant));
        Ok(Self {
            namespace: namespace.to_owned(),
            schema,
            policies,
            probe_set,
            approval_roles,
            actions,
            types,
            user_in_tenant,
            authorizer: cedar::Authorizer::new(),
            policy_set_hash: sha256_hex(policy_src),
            schema_hash: sha256_hex(schema_src),
        })
    }

    /// Evaluates the request against every grant in its chain (MILESTONE_18
    /// §8.3) and derives `ApprovalRequired` (ADR 0003 §4.4).
    ///
    /// - The Rust action check runs first for every grant
    ///   (`ActionNotGranted`); an action the schema does not declare is
    ///   `UnknownAction`.
    /// - Main evaluation, leaf first. Any diagnostic error, or any entity,
    ///   context, or request that fails schema validation, is
    ///   `Deny(PolicyError)` whatever Cedar decided.
    /// - All grants allow: `Allow` with the union of satisfied permits.
    /// - The first grant that denies decides. The reason is `PolicyDeny`
    ///   for the leaf and `ChainDeny { grant_id }` for an ancestor. If a
    ///   forbid determined the deny, or the request carried an approval,
    ///   that is the result.
    /// - Otherwise, for each `@approval` role in source order, the probe set
    ///   is evaluated for every grant with `context.approval` = {approver:
    ///   the placeholder user, plus the request's operation context fields}.
    ///   The placeholder has the role, the principal's tenant, and the
    ///   delegator's other attributes zeroed. If every grant allows,
    ///   `ApprovalRequired`; if the action's context rejects an approval
    ///   record, or no role succeeds, the deny stands. Probe errors are
    ///   `PolicyError`. The probe never yields `Allow`.
    pub fn evaluate_chain(&self, request: &AuthorizationRequest<'_>) -> Evaluation {
        self.evaluate(&Inputs::from_request(request))
    }

    pub(crate) fn evaluate(&self, inp: &Inputs<'_>) -> Evaluation {
        let mut ev = Evaluation {
            decision: Decision::deny(DenyReason::PolicyError),
            grants: Vec::new(),
            errors: Vec::new(),
        };
        if inp.chain.is_empty() {
            ev.decision = Decision::deny(DenyReason::NoGrant);
            return ev;
        }
        if inp.chain.iter().any(|g| !g.actions().contains(inp.action)) {
            ev.decision = Decision::deny(DenyReason::ActionNotGranted);
            return ev;
        }
        if !self.actions.contains(inp.action.as_str()) {
            ev.decision = Decision::deny(DenyReason::UnknownAction);
            return ev;
        }
        match self.evaluate_inner(inp, &mut ev) {
            Ok(d) => ev.decision = d,
            Err(e) => ev.errors.push(e),
        }
        ev
    }

    fn evaluate_inner(&self, inp: &Inputs<'_>, ev: &mut Evaluation) -> Result<Decision, String> {
        check_reserved(inp)?;
        let principal = self.uid(&self.types.agent, inp.agent.as_str());
        let action = self.uid(&self.types.action, inp.action.as_str());
        let resource = cedar_uid(inp.resource.uid())?;

        let mut base = self.base_entities(inp)?;
        let approval = match &inp.approval {
            None => None,
            Some(a) => {
                if a.approver.id() == inp.delegator.id() {
                    if a.approver != inp.delegator || a.tenant != inp.tenant {
                        return Err("approver conflicts with the delegator entity".into());
                    }
                } else {
                    base.push(self.user_entity(
                        a.approver.id().as_str(),
                        a.approver.role(),
                        a.approver.attrs(),
                        a.tenant,
                    )?);
                }
                let approver = self.uid(&self.types.user, a.approver.id().as_str());
                Some(approval_record(approver, a.fields)?)
            }
        };
        let entities = cedar::Entities::from_entities(base.clone(), Some(&self.schema))
            .map_err(|e| format!("entities: {e}"))?;
        let operation = pairs(inp.context)?;
        let grants = inp
            .chain
            .iter()
            .map(|g| record(&g.constraints().to_context_record()))
            .collect::<Result<Vec<_>, _>>()?;
        let parts = RequestParts {
            principal: &principal,
            action: &action,
            resource: &resource,
            operation: &operation,
        };

        let mut allow = BTreeSet::new();
        for (i, g) in inp.chain.iter().enumerate() {
            let req = self.request(&parts, &grants[i], approval.clone())?;
            let resp = self
                .authorizer
                .is_authorized(&req, &self.policies, &entities);
            let ge = grant_evaluation(g.grant_id(), &resp);
            ev.grants.push(ge.clone());
            if !ge.erroring_policies.is_empty() {
                ev.errors
                    .extend(resp.diagnostics().errors().map(|e| e.to_string()));
                return Ok(Decision::Deny {
                    reason: DenyReason::PolicyError,
                    determining_policies: ge.erroring_policies,
                });
            }
            if ge.allowed {
                allow.extend(ge.determining_policies);
                continue;
            }
            let reason = if i == 0 {
                DenyReason::PolicyDeny
            } else {
                DenyReason::ChainDeny {
                    grant_id: g.grant_id().clone(),
                }
            };
            if !ge.determining_policies.is_empty() {
                return Ok(Decision::Deny {
                    reason,
                    determining_policies: ge.determining_policies,
                });
            }
            if inp.approval.is_some() {
                return Ok(Decision::deny(reason));
            }
            return self.probe(inp, ev, &base, &parts, &grants, reason);
        }
        Ok(Decision::Allow {
            determining_policies: allow.into_iter().collect(),
        })
    }

    fn probe(
        &self,
        inp: &Inputs<'_>,
        ev: &mut Evaluation,
        base: &[cedar::Entity],
        parts: &RequestParts<'_>,
        grants: &[Expr],
        reason: DenyReason,
    ) -> Result<Decision, String> {
        let zeroed: Attrs = inp
            .delegator
            .attrs()
            .iter()
            .map(|(k, v)| (k.clone(), zeroed(v)))
            .collect();
        let placeholder = self.uid(&self.types.user, APPROVAL_PLACEHOLDER_ID);
        let approval = approval_record(placeholder, inp.context)?;
        'roles: for role in &self.approval_roles {
            let mut ents = base.to_vec();
            ents.push(self.user_entity(APPROVAL_PLACEHOLDER_ID, role, &zeroed, inp.tenant)?);
            let entities = cedar::Entities::from_entities(ents, Some(&self.schema))
                .map_err(|e| format!("probe entities: {e}"))?;
            for grant in grants {
                // The action's context does not accept this approval record:
                // no approval can help.
                let Ok(req) = self.request(parts, grant, Some(approval.clone())) else {
                    return Ok(Decision::deny(reason));
                };
                let resp = self
                    .authorizer
                    .is_authorized(&req, &self.probe_set, &entities);
                let erroring = sorted(resp.diagnostics().errors().map(error_policy));
                if !erroring.is_empty() {
                    ev.errors
                        .extend(resp.diagnostics().errors().map(|e| format!("probe: {e}")));
                    return Ok(Decision::Deny {
                        reason: DenyReason::PolicyError,
                        determining_policies: erroring,
                    });
                }
                if resp.decision() != cedar::Decision::Allow {
                    continue 'roles;
                }
            }
            return Ok(Decision::ApprovalRequired {
                requirement: ApprovalRequirement {
                    role: role.clone(),
                    action_digest: ActionDigest::compute(
                        inp.chain[0].grant_id(),
                        inp.action,
                        inp.resource.uid(),
                        inp.args,
                    ),
                },
            });
        }
        Ok(Decision::deny(reason))
    }

    fn uid(&self, ty: &cedar::EntityTypeName, id: &str) -> cedar::EntityUid {
        cedar::EntityUid::from_type_name_and_id(ty.clone(), cedar::EntityId::new(id))
    }

    fn user_entity(
        &self,
        id: &str,
        role: &str,
        attrs: &Attrs,
        tenant: &TenantId,
    ) -> Result<cedar::Entity, String> {
        let tenant = self.uid(&self.types.tenant, tenant.as_str());
        let mut a = HashMap::from([
            ("tenant".to_owned(), Expr::new_entity_uid(tenant.clone())),
            ("role".to_owned(), Expr::new_string(role.to_owned())),
        ]);
        for (k, v) in attrs {
            a.insert(k.clone(), expr(v)?);
        }
        let parents = if self.user_in_tenant {
            HashSet::from([tenant])
        } else {
            HashSet::new()
        };
        cedar::Entity::new(self.uid(&self.types.user, id), a, parents)
            .map_err(|e| format!("user entity: {e}"))
    }

    /// Principal, delegator, tenant, and resource.
    fn base_entities(&self, inp: &Inputs<'_>) -> Result<Vec<cedar::Entity>, String> {
        let tenant = self.uid(&self.types.tenant, inp.tenant.as_str());
        let delegator = self.uid(&self.types.user, inp.delegator.id().as_str());
        let agent = cedar::Entity::new(
            self.uid(&self.types.agent, inp.agent.as_str()),
            HashMap::from([
                ("tenant".to_owned(), Expr::new_entity_uid(tenant.clone())),
                ("delegator".to_owned(), Expr::new_entity_uid(delegator)),
            ]),
            HashSet::new(),
        )
        .map_err(|e| format!("agent entity: {e}"))?;
        let user = self.user_entity(
            inp.delegator.id().as_str(),
            inp.delegator.role(),
            inp.delegator.attrs(),
            inp.tenant,
        )?;
        let r = inp.resource;
        let resource = cedar::Entity::new(
            cedar_uid(r.uid())?,
            r.attrs()
                .iter()
                .map(|(k, v)| Ok((k.clone(), expr(v)?)))
                .collect::<Result<_, String>>()?,
            r.parents()
                .iter()
                .map(cedar_uid)
                .collect::<Result<_, _>>()?,
        )
        .map_err(|e| format!("resource entity: {e}"))?;
        Ok(vec![
            agent,
            user,
            cedar::Entity::new_no_attrs(tenant, HashSet::new()),
            resource,
        ])
    }

    fn request(
        &self,
        p: &RequestParts<'_>,
        grant: &Expr,
        approval: Option<Expr>,
    ) -> Result<cedar::Request, String> {
        let mut ctx = p.operation.clone();
        ctx.push(("grant".to_owned(), grant.clone()));
        if let Some(a) = approval {
            ctx.push(("approval".to_owned(), a));
        }
        let ctx = cedar::Context::from_pairs(ctx).map_err(|e| format!("context: {e}"))?;
        cedar::Request::new(
            p.principal.clone(),
            p.action.clone(),
            p.resource.clone(),
            ctx,
            Some(&self.schema),
        )
        .map_err(|e| format!("request: {e}"))
    }
}

struct RequestParts<'a> {
    principal: &'a cedar::EntityUid,
    action: &'a cedar::EntityUid,
    resource: &'a cedar::EntityUid,
    /// Operation context fields, without `grant` and `approval`.
    operation: &'a Vec<(String, Expr)>,
}

/// Borrowed evaluation inputs. Tests substitute fields the public request
/// cannot express (e.g. an approver from another tenant).
pub(crate) struct Inputs<'a> {
    pub agent: &'a AgentId,
    pub tenant: &'a TenantId,
    pub delegator: &'a Delegator,
    /// Leaf first.
    pub chain: &'a [Grant],
    pub action: &'a ActionId,
    pub resource: &'a ResolvedResource,
    pub args: &'a ValidatedArgs,
    pub context: &'a Attrs,
    pub approval: Option<ApprovalInput<'a>>,
}

pub(crate) struct ApprovalInput<'a> {
    pub approver: &'a Delegator,
    pub tenant: &'a TenantId,
    pub fields: &'a Attrs,
}

impl<'a> Inputs<'a> {
    pub(crate) fn from_request(r: &'a AuthorizationRequest<'_>) -> Self {
        let p = r.execution_context().principal();
        Self {
            agent: p.agent(),
            tenant: p.tenant(),
            delegator: p.delegator(),
            chain: r.execution_context().chain(),
            action: r.action(),
            resource: r.resource(),
            args: r.args(),
            context: r.context().fields(),
            // `ApprovalContext` carries no approver tenant, so the Cedar
            // `approver.tenant == principal.tenant` test always holds here.
            // #354 must reject cross-tenant approvers before building one.
            approval: r.approval().map(|a| ApprovalInput {
                approver: a.approver(),
                tenant: p.tenant(),
                fields: a.fields(),
            }),
        }
    }
}

fn cedar_uid(u: &EntityUid) -> Result<cedar::EntityUid, String> {
    let ty = cedar::EntityTypeName::from_str(u.entity_type())
        .map_err(|e| format!("entity type {:?}: {e}", u.entity_type()))?;
    Ok(cedar::EntityUid::from_type_name_and_id(
        ty,
        cedar::EntityId::new(u.id()),
    ))
}

fn expr(v: &AttrValue) -> Result<Expr, String> {
    Ok(match v {
        AttrValue::Bool(b) => Expr::new_bool(*b),
        AttrValue::Long(n) => Expr::new_long(*n),
        AttrValue::String(s) => Expr::new_string(s.clone()),
        AttrValue::Entity(u) => Expr::new_entity_uid(cedar_uid(u)?),
        AttrValue::Set(s) => Expr::new_set(s.iter().map(expr).collect::<Result<Vec<_>, _>>()?),
        AttrValue::Record(r) => record(r)?,
    })
}

fn pairs(attrs: &Attrs) -> Result<Vec<(String, Expr)>, String> {
    attrs
        .iter()
        .map(|(k, v)| Ok((k.clone(), expr(v)?)))
        .collect()
}

fn record(attrs: &Attrs) -> Result<Expr, String> {
    Expr::new_record(pairs(attrs)?).map_err(|e| format!("record: {e}"))
}

fn approval_record(approver: cedar::EntityUid, fields: &Attrs) -> Result<Expr, String> {
    if fields.contains_key("approver") {
        return Err("approval fields may not contain `approver`".into());
    }
    let mut p = pairs(fields)?;
    p.push(("approver".to_owned(), Expr::new_entity_uid(approver)));
    Expr::new_record(p).map_err(|e| format!("approval record: {e}"))
}

/// Same shape as `v`, with numbers 0, booleans false, strings and sets
/// empty; entity references are kept.
fn zeroed(v: &AttrValue) -> AttrValue {
    match v {
        AttrValue::Bool(_) => AttrValue::Bool(false),
        AttrValue::Long(_) => AttrValue::Long(0),
        AttrValue::String(_) => AttrValue::String(String::new()),
        AttrValue::Entity(u) => AttrValue::Entity(u.clone()),
        AttrValue::Set(_) => AttrValue::Set(BTreeSet::new()),
        AttrValue::Record(r) => {
            AttrValue::Record(r.iter().map(|(k, v)| (k.clone(), zeroed(v))).collect())
        }
    }
}

fn mentions_reserved(v: &AttrValue) -> bool {
    match v {
        AttrValue::Entity(u) => u.id().starts_with(RESERVED_ID_PREFIX),
        AttrValue::Set(s) => s.iter().any(mentions_reserved),
        AttrValue::Record(r) => r.values().any(mentions_reserved),
        AttrValue::Bool(_) | AttrValue::Long(_) | AttrValue::String(_) => false,
    }
}

/// Reserved ids (the approval placeholder) never come from the resolver or
/// the operation (ADR 0003 §4.4).
fn check_reserved(inp: &Inputs<'_>) -> Result<(), String> {
    let r = inp.resource;
    let reserved = r.uid().id().starts_with(RESERVED_ID_PREFIX)
        || r.parents()
            .iter()
            .any(|p| p.id().starts_with(RESERVED_ID_PREFIX))
        || r.attrs().values().any(mentions_reserved)
        || inp.context.values().any(mentions_reserved)
        || inp.delegator.attrs().values().any(mentions_reserved)
        || inp.chain.iter().any(|g| {
            g.constraints()
                .to_context_record()
                .values()
                .any(mentions_reserved)
        })
        || inp.approval.as_ref().is_some_and(|a| {
            a.fields.values().any(mentions_reserved)
                || a.approver.attrs().values().any(mentions_reserved)
        });
    if reserved {
        return Err(format!(
            "input uses the reserved id prefix {RESERVED_ID_PREFIX:?}"
        ));
    }
    Ok(())
}

fn to_policy_id(id: &cedar::PolicyId) -> PolicyId {
    // Every id in the set is an `@id` that passed `PolicyId::new` at load.
    PolicyId::new(id.to_string()).expect("policy ids are validated at load")
}

fn error_policy(e: &cedar::AuthorizationError) -> PolicyId {
    match e {
        cedar::AuthorizationError::PolicyEvaluationError(e) => to_policy_id(e.policy_id()),
    }
}

fn sorted(ids: impl Iterator<Item = PolicyId>) -> Vec<PolicyId> {
    ids.collect::<BTreeSet<_>>().into_iter().collect()
}

fn grant_evaluation(grant_id: &GrantId, resp: &cedar::Response) -> GrantEvaluation {
    let d = resp.diagnostics();
    GrantEvaluation {
        grant_id: grant_id.clone(),
        allowed: resp.decision() == cedar::Decision::Allow,
        determining_policies: sorted(d.reason().map(to_policy_id)),
        erroring_policies: sorted(d.errors().map(error_policy)),
    }
}

impl crate::traits::Sealed for CedarPdp {}

impl crate::traits::PolicyDecisionPoint for CedarPdp {
    fn authorize(&self, request: &AuthorizationRequest<'_>) -> Decision {
        self.evaluate_chain(request).decision
    }
    fn policy_set_hash(&self) -> &str {
        &self.policy_set_hash
    }
    fn schema_hash(&self) -> &str {
        &self.schema_hash
    }
}

#[cfg(test)]
mod tests;
