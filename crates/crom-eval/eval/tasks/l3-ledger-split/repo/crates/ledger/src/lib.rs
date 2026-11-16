//! Accounts and balanced journal entries.

use std::collections::BTreeMap;
use std::fmt;

use money::{Currency, Money, MoneyError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccountKind {
    Asset,
    Liability,
    Income,
    Expense,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub kind: AccountKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    pub account: String,
    pub amount: Money,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub memo: String,
    pub postings: Vec<Posting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    UnknownAccount(String),
    DuplicateAccount(String),
    Unbalanced { memo: String, residual: i64 },
    EmptyEntry,
    NoParticipants,
    Money(MoneyError),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::UnknownAccount(a) => write!(f, "unknown account {a}"),
            LedgerError::DuplicateAccount(a) => write!(f, "account {a} already exists"),
            LedgerError::Unbalanced { memo, residual } => {
                write!(f, "entry '{memo}' is unbalanced by {residual} minor units")
            }
            LedgerError::EmptyEntry => write!(f, "entry has no postings"),
            LedgerError::NoParticipants => write!(f, "split needs at least one participant"),
            LedgerError::Money(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<MoneyError> for LedgerError {
    fn from(e: MoneyError) -> Self {
        LedgerError::Money(e)
    }
}

pub struct Ledger {
    currency: Currency,
    accounts: BTreeMap<String, Account>,
    entries: Vec<Entry>,
}

impl Ledger {
    pub fn new(currency: Currency) -> Self {
        Ledger {
            currency,
            accounts: BTreeMap::new(),
            entries: Vec::new(),
        }
    }

    pub fn currency(&self) -> Currency {
        self.currency
    }

    pub fn open(&mut self, name: &str, kind: AccountKind) -> Result<(), LedgerError> {
        if self.accounts.contains_key(name) {
            return Err(LedgerError::DuplicateAccount(name.to_string()));
        }
        self.accounts.insert(
            name.to_string(),
            Account {
                name: name.to_string(),
                kind,
            },
        );
        Ok(())
    }

    pub fn accounts(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Record an entry. Postings must reference open accounts and sum to zero.
    pub fn post(&mut self, memo: &str, postings: Vec<Posting>) -> Result<(), LedgerError> {
        if postings.is_empty() {
            return Err(LedgerError::EmptyEntry);
        }
        let mut sum = Money::zero(self.currency);
        for p in &postings {
            if !self.accounts.contains_key(&p.account) {
                return Err(LedgerError::UnknownAccount(p.account.clone()));
            }
            sum = sum.checked_add(&p.amount)?;
        }
        if !sum.is_zero() {
            return Err(LedgerError::Unbalanced {
                memo: memo.to_string(),
                residual: sum.minor(),
            });
        }
        self.entries.push(Entry {
            memo: memo.to_string(),
            postings,
        });
        Ok(())
    }

    /// Record an expense paid by `payer` and shared among `participants`
    /// in proportion to their weights.
    ///
    /// Each participant's account is debited with their share and the
    /// payer's account is credited with the full amount.
    pub fn split_expense(
        &mut self,
        memo: &str,
        payer: &str,
        amount: Money,
        participants: &[(&str, u32)],
    ) -> Result<(), LedgerError> {
        if participants.is_empty() {
            return Err(LedgerError::NoParticipants);
        }
        let weights: Vec<u32> = participants.iter().map(|(_, w)| *w).collect();
        let shares = amount.allocate(&weights)?;
        let mut postings: Vec<Posting> = participants
            .iter()
            .zip(shares)
            .map(|((name, _), share)| Posting {
                account: name.to_string(),
                amount: share,
            })
            .collect();
        postings.push(Posting {
            account: payer.to_string(),
            amount: amount.negate(),
        });
        self.post(memo, postings)
    }

    pub fn balance(&self, account: &str) -> Result<Money, LedgerError> {
        if !self.accounts.contains_key(account) {
            return Err(LedgerError::UnknownAccount(account.to_string()));
        }
        let mut total = Money::zero(self.currency);
        for e in &self.entries {
            for p in &e.postings {
                if p.account == account {
                    total = total.checked_add(&p.amount)?;
                }
            }
        }
        Ok(total)
    }

    /// All account balances, sorted by account name.
    pub fn balances(&self) -> Result<Vec<(String, Money)>, LedgerError> {
        self.accounts
            .keys()
            .map(|name| Ok((name.clone(), self.balance(name)?)))
            .collect()
    }
}
