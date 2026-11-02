# EC2 systemd auto-deployment

> **Status:** This describes a future OIDC/SSM automation setup and is not the
> current deployment path. For each successful `main` build, manually deploy
> the GitHub Actions artifact using
> [`manual-ec2-deployment.md`](./manual-ec2-deployment.md). A push to `main`
> does not currently deploy to EC2 automatically.

This config targets an Ubuntu 22.04 x86_64 EC2 instance, matching the binary
built by `.github/workflows/build-linux.yml`. A merge to `main` builds the Rust
release binary, uploads an immutable release to a private S3 bucket, and asks
AWS Systems Manager (SSM) to install and restart the systemd service. GitHub
authenticates to AWS with OIDC; no long-lived AWS access key or EC2 SSH key is
stored in GitHub.

## 1. Prepare the EC2 instance

Use an x86_64 Ubuntu 22.04 instance. Attach an instance profile with:

- `AmazonSSMManagedInstanceCore`.
- `s3:GetObject` limited to `arn:aws:s3:::<DEPLOY_BUCKET>/releases/*`.
- The least-privilege S3 permissions required by the API for its separate
  production uploads bucket.

Ensure the SSM Agent is installed and the instance is online in Systems
Manager. Allow outbound HTTPS to SSM and S3 (or configure the corresponding
VPC endpoints). Do not expose port 8080 publicly.

Install the runtime tools:

```bash
sudo apt-get update
sudo apt-get install -y awscli ca-certificates curl
```

Create the production environment file before the first deployment:

```bash
sudo touch /etc/sketchflow-backend.env
sudo chown root:root /etc/sketchflow-backend.env
sudo chmod 0600 /etc/sketchflow-backend.env
sudoedit /etc/sketchflow-backend.env
```

Populate it with the production values from `.env.example`. At minimum set
`DATABASE_URL`, a strong `JWT_SECRET`, `HOST=127.0.0.1`, `PORT=8080`,
`PUBLIC_BASE_URL`, and exact production `CORS_ALLOWED_ORIGINS`. Configure
`S3_BUCKET` and `S3_REGION` for persistent uploads; configure Redis and payment
provider settings as required. Keep secrets in this root-owned file, not in
GitHub variables or the repository.

The deployment script creates the `sketchflow` service account and installs
the systemd unit during the first successful deployment.

## 2. Create the private deployment artifact bucket

Create an S3 bucket in the same AWS region. Keep Block Public Access enabled,
enable default encryption, and add a lifecycle rule to expire old
`releases/` objects after a retention period you choose. This bucket is only
for release artifacts; do not reuse the bucket that stores user uploads.

## 3. Configure GitHub-to-AWS OIDC

In IAM, add the GitHub Actions OIDC provider if the AWS account does not
already have it:

- Provider URL: `https://token.actions.githubusercontent.com`
- Audience: `sts.amazonaws.com`

Create a deploy role whose trust policy restricts `aud` to
`sts.amazonaws.com` and `sub` to:

```text
repo:SRICKKY/sketchflow-backend:ref:refs/heads/main
```

For the role's trust relationship, replace `<ACCOUNT_ID>` and use:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {
      "Federated": "arn:aws:iam::<ACCOUNT_ID>:oidc-provider/token.actions.githubusercontent.com"
    },
    "Action": "sts:AssumeRoleWithWebIdentity",
    "Condition": {
      "StringEquals": {
        "token.actions.githubusercontent.com:aud": "sts.amazonaws.com",
        "token.actions.githubusercontent.com:sub": "repo:SRICKKY/sketchflow-backend:ref:refs/heads/main"
      }
    }
  }]
}
```

Attach a permissions policy like the following, replacing the region, account,
instance, and bucket placeholders with the actual values:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "s3:PutObject",
      "Resource": "arn:aws:s3:::<DEPLOY_BUCKET>/releases/*"
    },
    {
      "Effect": "Allow",
      "Action": "ssm:SendCommand",
      "Resource": [
        "arn:aws:ssm:<AWS_REGION>::document/AWS-RunShellScript",
        "arn:aws:ec2:<AWS_REGION>:<ACCOUNT_ID>:instance/<EC2_INSTANCE_ID>"
      ]
    },
    {
      "Effect": "Allow",
      "Action": "ssm:GetCommandInvocation",
      "Resource": "*"
    }
  ]
}
```

Give this role only the permissions needed by the workflow. The SSM send
permission is limited to the shell document and target instance:

- `s3:PutObject` for
  `arn:aws:s3:::<DEPLOY_BUCKET>/releases/*`.
- `ssm:SendCommand` for the `AWS-RunShellScript` document in the selected
  region and the specific EC2 instance.
- `ssm:GetCommandInvocation` to retrieve the result of the command it starts.

Set these GitHub repository settings under **Settings → Secrets and variables
→ Actions**:

| Name | Type | Value |
|---|---|---|
| `AWS_DEPLOY_ROLE_ARN` | Secret | ARN of the OIDC deploy role |
| `AWS_REGION` | Variable | AWS region containing the instance and bucket |
| `EC2_INSTANCE_ID` | Variable | The target instance ID, such as `i-...` |
| `DEPLOY_BUCKET` | Variable | Name of the private deployment artifact bucket |

The EC2 instance must be managed by SSM and have permission to read the
deployment bucket prefix. The GitHub role uploads releases and invokes SSM; it
does not receive access to production runtime secrets.

## 4. Configure HTTPS and networking

Keep the EC2 security group closed to inbound port 8080. If the API needs a
public HTTPS endpoint, install a reverse proxy such as Caddy, allow inbound
ports 80/443, and proxy the API hostname to `127.0.0.1:8080`. Caddy supports
the API's WebSocket upgrade requests through `reverse_proxy`.

For example, the Caddy site block can be:

```text
backend.sketchflow.online {
    reverse_proxy 127.0.0.1:8080
}
```

## 5. Deploy and verify

The workflow runs on pull requests to `main` to validate the build. It deploys
only after a successful push to `main` (including a merged pull request).
`workflow_dispatch` can also deploy the selected `main` revision. Deployments
are serialized to avoid overlapping updates.

The deployment stages the binary under its commit SHA, switches the
`/opt/sketchflow-backend/current` symlink, restarts
`sketchflow-backend.service`, and checks `http://127.0.0.1:8080/health`. If
restart or health verification fails, it restores the prior release and marks
the GitHub Actions run as failed.

Check the service on EC2:

```bash
sudo systemctl status sketchflow-backend
sudo journalctl -u sketchflow-backend -n 100 --no-pager
curl --fail http://127.0.0.1:8080/health
```

Also verify the public HTTPS health URL after configuring DNS and the reverse
proxy. Keep database migrations backward-compatible: the API applies embedded
migrations at startup, and rolling back a binary cannot undo a database
migration.
