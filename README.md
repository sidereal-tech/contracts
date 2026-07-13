# sidereal contracts

Soroban smart contracts for [Sidereal](https://www.sidereal.tech), yield
tokenization on Stellar: split a yield-bearing position into a principal token
(PT) and a yield token (YT), trade either, recombine or redeem at maturity.

Live on Stellar mainnet since 2026-07-11. Not professionally audited; the
contracts are immutable, so treat the deployment as early and unaudited.

- Protocol design: [`ARCHITECTURE.md`](./ARCHITECTURE.md)
- Deployment manifests and provenance: [`deployments/`](./deployments/)
- Known findings: [`findings.md`](./findings.md)
- Security policy: [`SECURITY.md`](./SECURITY.md)
- App: <https://www.sidereal.tech> · frontend/SDK repo:
  [`sidereal-tech/web`](https://github.com/sidereal-tech/web) · docs repo:
  [`sidereal-tech/docs`](https://github.com/sidereal-tech/docs)

## Layout

| Path | Contents |
|---|---|
| `contracts/` | SY wrapper, tokenizer, PT/YT tokens, AMM, Blend adapter, shared types |
| `tests/integration` | Cross-contract integration tests |
| `scripts/` | Build, deploy, seed, and provenance-verification scripts |
| `deployments/` | Mainnet and testnet deployment manifests |

## Build and test

```bash
rustup target add wasm32v1-none   # SDK 26 wasm target
cargo install --locked stellar-cli

make test    # cargo test --workspace
make wasm    # optimized deployable wasm artifacts
make deploy  # deploy to testnet
```

CI runs the full test suite, builds every contract to wasm and rejects any
floating-point opcode, and asserts the build is byte-for-byte reproducible.

## History and provenance note

This repo was extracted from the original monorepo
([`PoulavBhowmick03/sidereal`](https://github.com/PoulavBhowmick03/sidereal))
at commit `3490b40` with history preserved via `git filter-repo`. Filtering
rewrites commit hashes, so the `source_commit` values recorded in
`deployments/*.toml` refer to commits in the **original** repository, which
remains the verification anchor for the mainnet provenance chain. Verify
deployed bytecode there, per `docs/deploy/PROVENANCE.md` in the docs repo.

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
