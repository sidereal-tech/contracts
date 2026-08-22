# Deployments

This directory is for committed public deployment manifests.

After a successful reproducible testnet deployment, commit:

```text
deployments/testnet.toml
```

The manifest must include the source commit, deployer public key, deployed
contract addresses, local Wasm hashes, and on-chain Wasm hashes. New V2
manifests include the strategy, vault, PT, YT, tokenizer, AMM, and resting
orderbook plus its initial taker fee and immutable fee recipient. Older
manifests legitimately omit the orderbook and must be treated as legacy by
clients. Do not store private keys or local CLI secrets here.
