# Bedrock virtualization field guide

The standalone guide is [index.html](index.html). Edit the chapter fragments
(`foundations.html`, `intel-deep.html`, `amd-deep.html`, and the other `*.html`
fragments) or `index.template.html`, then regenerate it with:

```sh
python3 docs/virtualization-guide/build.py
```

The build uses only Python's standard library. The generated guide has no
runtime dependency beyond a browser; its interactive diagrams use plain
JavaScript. If Google Fonts is unavailable, local fallback fonts are used.

From this worktree, run:

```sh
./docs/virtualization-guide/serve.sh 8765
```

The server listens only on `127.0.0.1`. On your laptop, forward the port over
SSH to the development host:

```sh
ssh -N -L 18765:127.0.0.1:8765 ubuntu@dev-dancline
```

Then open `http://127.0.0.1:18765/docs/virtualization-guide/` locally. The
first port is a port on your computer; if it is busy, choose another free
number on the left side of `-L` and in the browser URL. The second is the server port on
the development host. If your SSH hostname differs, replace `dev-dancline`
with the host you normally use. The server needs to serve the repository root
so the article's relative links to code and performance evidence resolve.

This is documentation only. It does not build or load a kernel module.
