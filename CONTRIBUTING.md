# Contributing

ZedXcode is a personal project maintained in spare time. Pull requests are
welcome, but for anything non-trivial please open an issue first so we can
agree on the approach before you write code.

## Dev setup

Follow [Dev install (from source)](README.md#dev-install-from-source) in the
README to build `xcode-dap` and install the dev extension.

## Before submitting

Run the checks from the README's [Development](README.md#development) section:

```sh
cargo test                                        # unit + integration tests
python3 tests/dap_smoke.py roundtrip              # DAP framing roundtrip vs the real binary
python3 tests/dap_smoke.py session --mock-pipeline  # full DAP session, no Xcode build needed
./install.sh                                      # symlink the built binary onto your PATH
```

## License

Contributions are licensed under [Apache-2.0](LICENSE) per Section 5 of the
license. No CLA.
