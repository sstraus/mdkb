---
id: 017-mq8r
title: Implement A/B experiment infrastructure
status: complete
priority: P3
created: 2026-01-31
updated: 2026-01-31
dependencies: ["014-jn5o", "015-ko6p"]
acceptance_criteria:
  - mdkb experiment create sets up A/B test with two configs
  - Traffic split configurable (default 50/50)
  - Experiment status shows metrics for each variant
  - mdkb experiment end concludes test with winner
  - Minimum sample size before significance
  - Experiments can compare configs (e.g., chunking strategy)
test_coverage: Experiment lifecycle tests
---

## Problem

No way to scientifically compare configuration changes. Can't measure if new RRF weights or chunking strategy improves results.

## Solution

Implement A/B testing infrastructure:

```bash
# Create experiment
mdkb experiment create "chunking-strategy" \
    --config-a '{"strategy":"fixed"}' \
    --config-b '{"strategy":"markdown"}'

# Check status
mdkb experiment status "chunking-strategy"
# Output:
# Experiment: chunking-strategy (running since 2026-01-15)
# Variant A (fixed): avg score 0.72, p95 latency 35ms, n=156
# Variant B (markdown): avg score 0.78, p95 latency 42ms, n=148
# Significance: 95% confidence B has better quality

# End experiment
mdkb experiment end "chunking-strategy"
```

Implementation:
1. On query, check active experiments
2. Route to variant A or B based on hash
3. Record results per variant
4. Calculate statistical significance

## Implementation Tasks

- [ ] Add experiments/experiment_results tables (in 014)
- [ ] Implement create_experiment()
- [ ] Implement get_experiment_variant() - consistent routing
- [ ] Route queries to appropriate config variant
- [ ] Record results per variant
- [ ] Calculate significance (two-sample t-test)
- [ ] Implement experiment_status()
- [ ] Implement experiment_end()
- [ ] Test: Consistent variant assignment
- [ ] Test: Results tracked per variant
- [ ] Test: Significance calculation correct

## Notes

From plan Phase 8 lines 1424-1448.

This is P3 because:
- Requires significant infrastructure
- Most users won't need formal A/B testing
- Useful for advanced optimization

Example use: Testing Q4_K_M vs Q5_K_M embedding quantization.
