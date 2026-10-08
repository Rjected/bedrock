# Trie-collapse load: the txgen image plus the RawStorage artifact, its deploy
# spec (deploy.yaml, run before the warm checkpoint) and the load spec
# (trie.yaml) and regression trigger specs; selected with TXGEN_SPEC.
FROM bedrock/tempo-txgen:latest
COPY *.yaml raw-storage.json /workload/trie/
