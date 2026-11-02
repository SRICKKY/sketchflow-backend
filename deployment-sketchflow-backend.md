# SketchFlow Backend Deployment Plan

## Goal

Deploy the Rust API to an EC2 instance and make it available at
`https://backend.sketchflow.online`. Use Amazon RDS for PostgreSQL and Layerbase
for managed Redis. The current deployment process is manual: pushes to `main`
build a Linux artifact, which is then copied to EC2 using the procedure in
[`docs/manual-ec2-deployment.md`](./docs/manual-ec2-deployment.md).

## Proposed architecture

```text
SketchFlow UI
    |
    | HTTPS / WSS
    v
backend.sketchflow.online
    |
    | DNS A record -> Elastic IP
    v
EC2 (Ubuntu 22.04 x86_64)
    |-- Nginx: TLS termination and reverse proxy
    |-- Rust API: 127.0.0.1:8080
    |-- SSM Agent: administration and deployment
    |
    |-- private connection --> RDS PostgreSQL
    |-- outbound TLS -------> Layerbase Redis
    |-- instance role ------> private S3 bucket for assets
```

The app applies embedded SQL migrations during startup. The GitHub Actions
workflow builds and uploads an x86_64 Linux release binary; it does not
currently deploy to EC2 automatically. OIDC/SSM automation is deferred.
EC2/systemd provisioning steps are in
[`docs/ec2-systemd-deployment.md`](./docs/ec2-systemd-deployment.md).

## Plan

### 1. Confirm AWS region, cost, and instance compatibility

- Choose an AWS region near the expected users and verify Layerbase supports
  connections from it.
- Select an x86_64 EC2 instance running Ubuntu 22.04 to match the current
  `ubuntu-22.04` GitHub Actions build environment. If the OS or architecture
  changes, build the binary in a compatible environment and verify it on the
  target host before production.
- Check current EC2 and RDS pricing, free-tier eligibility, storage, backups,
  data transfer, and public IPv4 charges. Do not assume EC2 or RDS will be
  free; configure an AWS budget alert before provisioning.
- Start with one EC2 instance. This is a single point of failure; document the
  expected maintenance and recovery process before treating it as highly
  available.

### 2. Provision the EC2 host and access controls

- Create the EC2 instance in a VPC with an Elastic IP so its address remains
  stable for DNS.
- Prefer AWS Systems Manager Session Manager for administration and deployment.
  Attach an instance profile with SSM permissions and the narrowly scoped S3
  access described below. Avoid a publicly open SSH port; if SSH is temporarily
  required, restrict it to a trusted source and remove it afterward.
- Configure the instance security group to allow inbound HTTPS (443) and HTTP
  (80, for certificate issuance/redirects). Do not expose API port 8080,
  PostgreSQL 5432, or Redis publicly.
- Install and configure Nginx and a `systemd` unit for the API.
  Store the service binary under `/opt/sketchflow-backend/` and run it as a
  dedicated unprivileged user.
- Configure Nginx and Certbot to obtain and renew TLS for `backend.sketchflow.online`,
  redirect HTTP to HTTPS, and proxy HTTP and WebSocket upgrade requests to the
  local API listener.

### 3. Create the production PostgreSQL database

- Provision an RDS PostgreSQL instance in private subnets, with public access
  disabled, encryption enabled, and automated backups/retention configured.
- Set the RDS security group to accept PostgreSQL traffic only from the EC2
  security group.
- Create a dedicated application database and least-privilege application
  credentials. Keep the master credentials separate from the application
  runtime.
- Require TLS for database connections and verify the SQLx connection's
  certificate/SSL configuration against the selected RDS setup.
- Store the production `DATABASE_URL` in AWS Systems Manager Parameter Store
  as a `SecureString` or AWS Secrets Manager. Do not commit it or put it in a
  GitHub Actions log.
- Treat the initial database as a new backend schema. Do not point these
  migrations at the existing Next.js/Neon database without a separately
  reviewed schema and data migration plan.

### 4. Configure Layerbase Redis

- Create the Layerbase Redis instance and verify the free-plan limits, backup
  and eviction behavior, connection limits, region, and TLS requirements for
  the expected collaboration workload.
- Store its authenticated connection URL in the instance's secure runtime
  configuration. Use the provider's TLS URL (`rediss://`) when available and
  verify the connection from the EC2 VPC/host.
- Keep Redis optional during initial deployment only if degraded collaboration
  behavior is acceptable. Test that multi-instance collaboration fan-out works
  with the configured Redis service before enabling any additional API
  instances.

### 5. Configure S3 for user-uploaded assets

- Create a private S3 bucket for production assets, with block-public-access
  enabled, encryption enabled, and versioning/lifecycle settings selected for
  the recovery and cost requirements.
- Attach an EC2 instance-role policy limited to that bucket and the required
  object operations. The AWS SDK already supports the instance role credential
  provider, so do not configure long-lived AWS access keys on the instance.
- Set `S3_BUCKET` and `S3_REGION`; leave `S3_ENDPOINT_URL` unset to use AWS S3.
  Keep the Silo service for local development only.
- Confirm upload, download, and deletion behavior against the production bucket.
  Avoid relying on the local filesystem fallback for persistent production
  uploads.

### 6. Configure DNS, API settings, and frontend routing

- Add an A record for `backend.sketchflow.online` pointing to the EC2 Elastic IP.
- Configure the API runtime with production values, including:
  - `HOST=127.0.0.1` and `PORT=8080` behind Nginx.
  - `PUBLIC_BASE_URL=https://backend.sketchflow.online`.
  - `CORS_ALLOWED_ORIGINS` set to the exact production UI origin(s), for
    example `https://sketchflow.online` and `https://www.sketchflow.online`
    only if both are actually used.
  - `DATABASE_URL`, `REDIS_URL`, `JWT_SECRET`, `S3_BUCKET`, `S3_REGION`, and
    any required payment-provider secrets.
- Store runtime secrets in Parameter Store or Secrets Manager and let the EC2
  instance role retrieve only the required parameters. Never put secrets in
  the deployment artifact or repository.
- Set the UI's production API base URL to `https://backend.sketchflow.online`
  and make collaboration use `wss://backend.sketchflow.online`.
- Before switching production traffic, verify how the UI resolves relative
  paths such as `/api/uploads/{id}`. Either configure the UI host to proxy
  `/api/*` (including WebSocket upgrades) to the backend, or ensure the UI
  builds API and asset URLs against the backend origin. The API subdomain and
  UI domain are same-site, but they are different origins: credentialed CORS
  and the UI's credentials setting still need to be correct for session
  cookies.

### 7. Deploy changes from `main` (current manual process)

- A successful push to `main` triggers the **Build Linux release** workflow.
  Pull requests build for validation; the workflow uploads an artifact named
  `sketchflow-backend-linux-x86_64`.
- Download the artifact for the successful `main` run, copy it to EC2 with
  `scp`, install it as a new versioned release, restart systemd, and verify
  local and public health endpoints. Follow the complete procedure in
  [`docs/manual-ec2-deployment.md`](./docs/manual-ec2-deployment.md).
- Automated OIDC/SSM deployment is deferred. The guide in
  [`docs/ec2-systemd-deployment.md`](./docs/ec2-systemd-deployment.md) describes
  that future setup, not the current release process.

### 8. Validate before directing production traffic

- Confirm DNS resolution, valid TLS, HTTP-to-HTTPS redirect, and no public
  access to port 8080 or database ports.
- Confirm `/health`, `/docs`, authentication/session cookies, and requests
  from each configured UI origin.
- Test file uploads and reads through S3, Redis-backed collaboration across
  browser sessions, and WebSocket reconnects.
- Verify startup migrations against a disposable/staging RDS database first.
  Back up production before applying a migration that could be destructive or
  incompatible with the running version.
- Point the production UI to the backend only after REST, asset URLs,
  credentialed CORS, and WebSockets work together over HTTPS.

### 9. Operations and recovery

- Enable CloudWatch instance/system logs and alarms for EC2 health, disk usage,
  API health-check failures, RDS storage/connections, and budget thresholds.
- Regularly test RDS restore procedures and retain the prior deployment binary
  until the new release is verified.
- Document how to rotate database, Redis, JWT, and payment secrets; how to
  redeploy or roll back; and how to rebuild the EC2 instance from a clean image.

## Deployment completion criteria

- `https://backend.sketchflow.online/health` returns a healthy response with a
  valid certificate.
- A successful main-branch build artifact can be manually deployed and verified
  on EC2 by following the runbook.
- The API connects only to private RDS, Layerbase Redis over the configured
  secure connection, and the private S3 bucket via its instance role.
- The production UI can authenticate, load assets, and establish collaboration
  WebSockets using the intended API routing.
- Backups, alarms, deployment rollback, and current AWS/provider costs have
  been reviewed.

## Project references

- [`README.md`](./README.md) — local services, endpoints, migrations, and
  production-related behavior.
- [`.env.example`](./.env.example) — runtime configuration names.
- [`compose.yaml`](./compose.yaml) — local-only PostgreSQL, Redis, and Silo
  services.
- [`.github/workflows/build-linux.yml`](./.github/workflows/build-linux.yml) —
  current x86_64 release build and artifact upload.
- [GitHub Actions OIDC for AWS](https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-deployments/configuring-openid-connect-in-amazon-web-services)
- [AWS Systems Manager Session Manager](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager.html)
- [AWS Systems Manager Run Command](https://docs.aws.amazon.com/systems-manager/latest/userguide/run-command.html)
