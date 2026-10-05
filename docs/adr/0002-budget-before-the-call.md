# ADR 0002 — The budget is checked before the call, with a pessimistic estimate

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

The cost of a call to a provider is `input_tokens × input_price + output_tokens ×
output_price`. The input tokens can be counted beforehand, because they are in the request
body (with a tokenizer, which is a serious problem). The output ones **are not known in
advance**: they depend on the model and on the question.

The easy case is therefore the answer: you look at `usage` and do the math. The easy case is
also the one that is not needed, because by the time the answer is there, the money is gone.
A runaway doing 4,000 requests in an hour does not stop at the cent.

## Decision

**Reservation first, settlement after.** As in `agentloop`, and for the same reason: a cap
that is checked afterwards protects the previous request.

The estimate is made of two parts:

```
estimate = estimated_input_tokens × input_price + maximum_output × output_price
```

- `estimated_input_tokens`: `body_length / 4`, rounded up. It is the standard token estimate,
  and rounding up means it **never underestimates**. The divisor is calibrated on the
  tokenizers of the common models: if a tenant sends Italian text or code the ratio is
  higher, and rounded up it still holds as a cap.
- `maximum_output`: read from `max_tokens` if present, otherwise a configuration constant.
  **The default is deliberately high**: if the client does not say how much it wants to
  generate, the worst is assumed. Underestimating here means not protecting.

If the reservation does not fit the tenant's remaining budget, a `429` is returned and **the
provider is not called**: it is the behavior that distinguishes a cap from a count.

The settlement uses the real `usage` and releases the difference. If the response does not
carry `usage`, it settles with the estimate: that is the least wrong thing to do when you do
not know.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| check only afterwards | exact, zero false positives | protects the previous request | it is not a cap, it is a report |
| count tokens with an exact tokenizer | precise estimate | needs the model's tokenizer; a new model invalidates it | the dependency grows more than the accuracy gained |
| average tokens per tenant | zero cost | a tenant with long prompts gets through until it is too late | it moves the problem, it does not solve it |
| quota in requests | very simple | one 200k-token prompt costs more than a hundred 500-token ones | it does not measure the resource being billed |

## Consequences

**We win:**
- the cap holds in front of the spending, and the reservation is the only mechanism that
  makes that true;
- no rejection for a tenant asking for less than expected: the rounded-up estimate generates
  rare, not frequent, false positives;
- a single mechanism across the portfolio (`agentloop` and here), so it is explained once.

**We pay:**
- **false positives**: a tenant with long prompts and a generous `max_tokens` may see a
  request rejected that would actually cost little. The remedy is declaring `max_tokens`,
  which is good hygiene on the other side;
- the estimate on the divisor 4 is coarse. It is fine for a cap, bad for a quote.

## Verification

The test that matters: a tenant 1 ¢ from the cap sending a 1-token request must pass; the same
tenant sending a 100,000-token one must receive `429` and the fake provider must have received
**zero** requests.
