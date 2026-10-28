# Security policy

Do not open a public issue for a security report.

Report vulnerabilities in vxedge privately through GitHub's private vulnerability reporting:
open the repository's **Security** tab and choose **Report a vulnerability**
(https://github.com/vxedge/vxedge/security/advisories/new).

This repository covers the control plane (`vxedged`), the vxAPI surface, the CLI, and gateway
provisioning. `vxedged` runs privileged (under sudo or with `CAP_NET_ADMIN`), so bugs that cross
that privilege boundary are in scope.

Supported versions, response targets, and scope notes are in the vxedge organization
security policy: https://github.com/vxedge/.github/blob/main/SECURITY.md
