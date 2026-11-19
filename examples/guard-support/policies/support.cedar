// Guard policy set v1 for the support reference deployment (ADR 0003).
// Validated in strict mode against support.cedarschema by
// scripts/validate-guard-policies.sh. Guard denies on any evaluation error,
// so an erroring forbid cannot fail open.
@id("tenant-wall")
forbid (principal, action, resource)
unless { resource.tenant == principal.tenant };

@id("read-delegated-customers")
permit (
  principal is Support::Agent,
  action in [Support::Action::"customer.read", Support::Action::"invoice.list"],
  resource is Support::Customer
)
when { context.grant.customers.contains(resource) };

@id("refund-within-limits")
permit (
  principal is Support::Agent,
  action == Support::Action::"refund.issue",
  resource is Support::Invoice
)
when
{
  context.grant.customers.contains(resource.customer) &&
  context.amount_cents > 0 &&
  context.amount_cents <= context.grant.max_refund_cents &&
  context.amount_cents <= principal.delegator.refund_limit_cents &&
  context.amount_cents + resource.refunded_cents <= resource.amount_cents
};

// Guard probes this policy with a placeholder approval to derive
// ApprovalRequired; the placeholder approver has role = the annotation value
// and tenant = principal.tenant. Only a stored approval ever authorizes.
@id("refund-with-supervisor-approval")
@approval("supervisor")
permit (
  principal is Support::Agent,
  action == Support::Action::"refund.issue",
  resource is Support::Invoice
)
when
{
  context.grant.customers.contains(resource.customer) &&
  context.amount_cents > 0 &&
  context has approval &&
  context.approval.approver.role == "supervisor" &&
  context.approval.approver.tenant == principal.tenant &&
  context.approval.amount_cents == context.amount_cents &&
  context.amount_cents + resource.refunded_cents <= resource.amount_cents
};

@id("no-bulk-export")
forbid (
  principal,
  action == Support::Action::"customer.export",
  resource
);
