# Trie-collapse load: the txgen image plus the RawStorage artifact, its deploy
# spec (deploy.yaml, run before the warm checkpoint) and the load spec
# (trie.yaml); selected with TXGEN_SPEC.
FROM bedrock/tempo-txgen:latest
COPY deploy.yaml trie.yaml raw-storage.json /workload/trie/
