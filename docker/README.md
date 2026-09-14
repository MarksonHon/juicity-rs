# Docker server

The image packages the existing musl server binaries for `linux/amd64` (baseline
x86-64), `linux/arm64`, and `linux/arm/v7`. It contains only the server binary,
runs as UID/GID `65532:65532`, and needs no compiler or shell at runtime.

## Compose

After a stable release has published `ghcr.io/juicity/juicity-rs:<tag>`:

1. Copy `server.example.json` to `server.json` in this directory. Replace the
   example UUID and password with your own credentials.
2. Place your PEM certificate chain and private key at `certs/server.crt` and
   `certs/server.key`. Grant UID/GID 65532 read access to the files and traversal
   access to their directories (for example, group 65532 with mode 640 for the
   key). Keep the private key out of version control. Self-signed certificates
   require client certificate pinning or an explicitly configured trust policy.
3. Run `JUICITY_VERSION=<tag> docker compose -f docker/compose.yml up -d` from the
   repository root. Set the same variable for subsequent Compose commands.

The example uses Linux host networking and listens on UDP 23182. Allow that port
in your firewall; keep it consistent with `listen`. Docker Desktop requires host
networking support to be enabled. For bridge networking, remove `network_mode`
and publish `23182:23182/udp` instead. Stop with `JUICITY_VERSION=<tag> docker compose -f docker/compose.yml down`
from the root. SIGTERM gets 15 seconds to finish graceful shutdown.

The image defaults to `run -c /etc/juicity/server.json`; existing external config
and certificate mounts can be reused if their container paths match the JSON.
Use `docker run --rm ghcr.io/juicity/juicity-rs:<tag> --version` to inspect a build.
No config, credentials or certificate is embedded in the image.

## Local packaging and validation

Extract the existing musl artifacts so that only their server binaries are needed
at the following paths:

| Artifact | Build context path |
| --- | --- |
| `juicity-x86_64-unknown-linux-musl` | `dist/linux/amd64/juicity-server` |
| `juicity-aarch64-unknown-linux-musl` | `dist/linux/arm64/juicity-server` |
| `juicity-armv7-unknown-linux-musleabihf` | `dist/linux/arm/v7/juicity-server` |

For one architecture:

```sh
docker buildx build --platform linux/amd64 --load \
  --build-arg VERSION=<tag> --build-arg REVISION=<commit> \
  -t juicity-container:test .
bash docker/smoke-test.sh juicity-container:test linux/amd64 <commit>
```

Repeat for the other two platforms. Cross-architecture execution requires QEMU
or equivalent binfmt support. The smoke test needs Bash, OpenSSL and Docker. It
uses `busybox:1.37.0` to inspect the target's actual UDP socket table in its network
namespace, checks missing config/certificate/key failures, and requires exit code
0 after SIGTERM. The temporary test credentials and containers are removed.

## CI and publication

`Test Core` reuses its existing three musl artifacts, packages and tests each
architecture, and saves the tested images. PR workflows never log into GHCR or
push images. Docker-related file changes also trigger this workflow.

The existing manual `Release` workflow uses the selected tag's exact source
commit. After its assets are attached and all three container tests pass, the
publication job requires a non-draft, non-prerelease GitHub Release, loads the
saved images, and publishes `:<tag>-amd64`, `:<tag>-arm64`, `:<tag>-armv7` plus the
multi-platform `:<tag>` manifest. Only the current latest stable release updates
`:latest`. Only this publication job has `packages: write`; no PAT is needed.
The package is linked to this repository by its OCI source label. Maintainers
may need to set package visibility and grant the repository package access for
the first publication.
