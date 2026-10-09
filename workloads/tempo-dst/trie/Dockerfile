# Trie load: the txgen image plus the RawStorage artifact and its deploy spec
# (deploy.yaml, run before the warm checkpoint). tempo-dst generates each
# run's write spec and mounts it at /workload/trie/generated.yaml.
FROM bedrock/tempo-txgen:latest
COPY deploy.yaml raw-storage.json /workload/trie/
