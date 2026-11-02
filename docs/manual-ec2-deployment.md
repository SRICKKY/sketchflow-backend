# Manual EC2 deployment from `main`

This is the current production deployment process for
`backend.sketchflow.online`. A successful push to `main` starts the
**Build Linux release** GitHub Actions workflow and uploads an artifact. It does
not deploy to EC2 automatically; deployment automation through AWS OIDC/SSM is
deferred. The artifact is retained by GitHub for 14 days.

The steps below assume the existing Ubuntu x86_64 EC2 instance, systemd service,
Nginx reverse proxy, and production environment file are already configured.
They replace only the backend binary. Do not overwrite
`/etc/sketchflow-backend.env` during a routine release.

## 1. Wait for the `main` build

After the change has been merged or pushed to `main`, run this on your
development machine:

```bash
gh run list -R SRICKKY/sketchflow-backend \
  --workflow "Build Linux release" \
  --branch main \
  --limit 10
```

Choose the successful `push` run for the commit you intend to deploy. Check its
details and record the run ID:

```bash
RUN_ID=REPLACE_WITH_SUCCESSFUL_RUN_ID
gh run view "$RUN_ID" -R SRICKKY/sketchflow-backend \
  --json status,conclusion,event,headBranch,headSha,url
```

Proceed only when the run is completed successfully, its branch is `main`, and
its commit SHA is the intended release.

## 2. Download and verify the Linux artifact

Still on your development machine:

```bash
ARTIFACT_DIR="$HOME/Downloads/sketchflow-backend-$RUN_ID"
gh run download "$RUN_ID" -R SRICKKY/sketchflow-backend \
  --name sketchflow-backend-linux-x86_64 \
  --dir "$ARTIFACT_DIR"

file "$ARTIFACT_DIR/sketchflow-backend"
chmod +x "$ARTIFACT_DIR/sketchflow-backend"
shasum -a 256 "$ARTIFACT_DIR/sketchflow-backend"
```

`file` should identify the binary as an x86-64 Linux ELF executable. Keep the
SHA-256 output so it can be compared on EC2 after copying.

## 3. Copy the executable to EC2

The current Elastic IP is `16.179.0.71`. Adjust it if the address changes. Use
the SSH key configured for this instance:

```bash
scp -i "$HOME/Downloads/rust-backend-ec2.pem" \
  "$ARTIFACT_DIR/sketchflow-backend" \
  ubuntu@16.179.0.71:/tmp/sketchflow-backend.new
```

If SSH is restricted to your current public IP, make sure that IP is allowed
in the EC2 security group before connecting.

## 4. Install the release and restart the service

SSH to EC2:

```bash
ssh -i "$HOME/Downloads/rust-backend-ec2.pem" ubuntu@16.179.0.71
```

Record the current release path before switching, so it is available for
rollback in the same SSH session. Then install the new binary in a
versioned release directory and atomically update the `current` symlink:

```bash
TARGET=/opt/sketchflow-backend
PREVIOUS_RELEASE="$(readlink -f "$TARGET/current")"
test -x "$PREVIOUS_RELEASE/sketchflow-backend" || {
  echo "Could not find the currently deployed release; stop here."
  exit 1
}

RELEASE_ID="$(date -u +%Y%m%dT%H%M%SZ)"
RELEASE_DIR="$TARGET/releases/manual-$RELEASE_ID"
sudo install -d -o root -g root -m 0755 "$RELEASE_DIR"
sudo install -o root -g root -m 0755 \
  /tmp/sketchflow-backend.new "$RELEASE_DIR/sketchflow-backend"

sha256sum "$RELEASE_DIR/sketchflow-backend"
sudo ln -sfn "$RELEASE_DIR" "$TARGET/current.next"
sudo mv -Tf "$TARGET/current.next" "$TARGET/current"
sudo systemctl restart sketchflow-backend
```

Compare the EC2 `sha256sum` output with the value from step 2. Verify systemd,
the local health endpoint, and the public HTTPS endpoint:

```bash
sudo systemctl status sketchflow-backend --no-pager
sudo journalctl -u sketchflow-backend --since "5 minutes ago" --no-pager
curl --fail http://127.0.0.1:8080/health
curl --fail https://backend.sketchflow.online/health
```

Both health checks should return `{"status":"ok"}`. Keep the SSH session open
until the checks pass; the `PREVIOUS_RELEASE` variable is used for rollback.

## 5. Roll back if verification fails

If the service does not start or either health check fails, restore the prior
release from the same SSH session:

```bash
sudo ln -sfn "$PREVIOUS_RELEASE" "$TARGET/current.rollback"
sudo mv -Tf "$TARGET/current.rollback" "$TARGET/current"
sudo systemctl restart sketchflow-backend
sudo systemctl status sketchflow-backend --no-pager
sudo journalctl -u sketchflow-backend -n 100 --no-pager
```

Rolling back the binary does not undo a database migration. Keep schema
migrations backward-compatible and take a database backup before deploying a
change that may be destructive or incompatible.

## Operational notes

- The service reads production settings from
  `/etc/sketchflow-backend.env`; routine binary deployment does not require
  editing or copying that file.
- The backend runs its embedded migrations when it starts. Do not manually
  rerun migration SQL as part of this procedure.
- Keep port `8080` private. Nginx terminates HTTPS and proxies to the local
  backend.
- Retain the previous release until the new one has been verified. Do not
  remove old release directories as part of the deployment itself.
- The OIDC/SSM setup in
  [`ec2-systemd-deployment.md`](./ec2-systemd-deployment.md) describes a future
  automated deployment path; it is not required for this manual procedure.
