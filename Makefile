.PHONY: fmt test build sample-index sample-search sample-reverse clean-sample

fmt:
	cargo fmt --all --check

test:
	cargo test --workspace

build:
	cargo build --workspace

sample-index:
	cargo run -p geo-ingest -- prepare --config countries/colombia.toml --input data/sample/colombia.csv --output work/colombia-index

sample-search: sample-index
	cargo run -p geo-ingest -- stats --index work/colombia-index

sample-reverse: sample-index
	cargo run -p geo-api -- --config countries/colombia.toml --index work/colombia-index --bind 127.0.0.1:8080

clean-sample:
	rm -rf work/colombia-index
