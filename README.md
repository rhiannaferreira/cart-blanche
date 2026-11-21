# Cart Blanche

A Rust pipeline for the Builders Buildathon "Shopping Sessions" track (Databricks).
It reads the click-stream CSVs in `/data`, applies the rules in `rules.md`, and writes
`/output/user_daily/` and `/output/campaign_daily/` as Parquet.

    docker build -t cart-blanche .
    python3 scripts/run_fixtures.py fixtures -- ./target/release/shopping-sessions

MIT licensed.
