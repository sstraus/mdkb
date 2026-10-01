# Mutation gate, 2026-10-01 (mdkb, base 38282e5)

Engine: cargo-mutants on the rb box, one source file per run (`--lib`), 9042 mutants in the crate.
Order: recent landings (store/search.rs build_recall_query, 5e571e0), then the crate by module.
Survivors are classified with adversarial-tdd `mutants.py --classify` and filed as wiz stories;
no production code is changed on this branch.

| Unit | Mutants | Caught | Missed | Timeout | Unviable |
|---|---|---|---|---|---|
| src/store/search.rs (build_recall_query) | running | | | | |
