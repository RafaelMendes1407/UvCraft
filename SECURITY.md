# Security policy

## Reporting a vulnerability

Please avoid posting exploit details, credentials or private user data in a public
issue. If the repository's **Security** tab offers **Report a vulnerability**,
use that private GitHub flow. If it does not, use a private contact channel that
you have independently verified with the maintainer. This repository does not
publish a verified security email address.

Include the affected commit or version, the steps needed to reproduce the
issue (for a malformed mesh, attach the smallest file that triggers it),
expected and actual behavior, and the impact. Do not send real secrets or
personal data. Maintainers should acknowledge reports privately, assess
severity and affected releases, and coordinate a fix and disclosure with the
reporter. No response-time or backport guarantee is stated here.

## Scope

UvCraft opens mesh files (OBJ, glTF/GLB, FBX) and command scripts that may come
from untrusted sources; crashes, hangs, excessive memory use or file access
outside the asset's own directory caused by such input are in scope. The
JSON-RPC server (`uvcraft serve`) has no authentication and executes `load`
and `save` with the server's file permissions, so, like the Blender bridge,
it refuses to bind to anything but a loopback address. Any local process can
still connect to it.

## Supported versions

Continuous security checks cover the current `main` branch and proposed
changes. Whether an older binary receives a fix is decided for each report;
do not assume a release is supported merely because it remains downloadable.

The technical audit process is documented in [docs/SECURITY_AUDIT.md](docs/SECURITY_AUDIT.md).
