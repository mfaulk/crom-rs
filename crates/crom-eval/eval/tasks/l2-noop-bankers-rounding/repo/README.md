# ledger

Invoice arithmetic for the billing service.

## Rounding policy

Amounts are tracked internally in **mills** (1/10 of a cent) and rounded to
whole cents only when a line total is produced. Rounding is **half to even**
("banker's rounding"), as required by the finance team's audit policy, so
that systematic rounding bias cancels out across many invoice lines:

| mills | cents |
|------:|------:|
| 25    | 2     |
| 35    | 4     |
| 15    | 2     |
| -25   | -2    |

Do not change this to half-up rounding.

## Splitting

`split(total_cents, n)` divides a total into `n` shares that sum exactly to the
total. Shares differ by at most one cent; the leftover cents go to the
**first** payers, so `split(1000, 3) == [334, 333, 333]`.
