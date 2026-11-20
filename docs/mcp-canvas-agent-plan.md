# MCP Server for Canvas Agents — Implementation Plan

**Status:** Backend MVP implemented on `feat/mcp-canvas-agent`; integration with
external MCP clients and deployment validation remain.

## Goal

Allow MCP-compatible AI clients to discover a user's SketchFlow boards, read
board context, and create or edit canvas elements. Agent changes must be
validated, permission-checked, revisioned, and visible to people already
watching the board.

“Draw” means creating editable SketchFlow elements (shapes, lines, arrows,
freehand strokes, and text), not sending arbitrary pixels or controlling a
user's browser.

## Existing foundation

- The Rust backend owns board access checks and the persisted board document.
- Editor endpoints require a signed-in user and check the user's board role.
- `editor::store` applies revisioned operations with idempotency and conflict
  handling.
- Committed editor operations are broadcast to connected collaboration clients.
- The frontend canvas can render and edit the supported SketchFlow element
  types.
- Existing authentication is based on an HTTP-only browser session cookie.
  That is not by itself a suitable credential flow for external MCP clients.

Relevant implementation areas:

- `src/routes/editor.rs` — board details and revisioned editor writes.
- `src/editor/store.rs` — canonical board writer and revision handling.
- `src/routes/collaboration.rs` — element validation and live broadcasts.
- `src/auth/session.rs` — current browser-cookie authentication.
- `../sketchflow-ui/src/features/canvas/engine/CanvasEngine.ts` — canvas
  element behavior and rendering integration.

## Proposed architecture

1. **Host MCP in the Rust backend.** Use RMCP Streamable HTTP at `/api/mcp`,
   with stateless requests and origin/host validation. Keep transport handling
   separate from board-domain operations.
2. **Use a typed domain layer.** MCP tools call shared backend services for
   authentication, access control, element validation, revisioned writes, and
   collaboration broadcasts. They must not write database rows directly or
   reimplement a second board writer.
3. **Authenticate agents explicitly.** Add OAuth 2.1 authorization-code with
   PKCE for MCP clients, linking each grant to an existing SketchFlow user.
   Do not ask agents to copy browser cookies, expose session JWTs, or put
   long-lived secrets in tool arguments. Persist revocable grants and enforce
   short-lived access tokens and narrowly scoped permissions.
4. **Preserve user permissions.** Every board tool call resolves the
   authenticated user and checks current board access. Read tools require
   viewer access; write tools require editor access. Public-share links do not
   grant agent write permission.
5. **Commit through the existing revisioned path.** A successful agent
   operation should receive a client operation ID, validate a bounded batch,
   commit against the caller's base revision, and broadcast the committed
   operation so open canvases update through their existing collaboration
   connection.

## End-to-end flow

```mermaid
sequenceDiagram
    actor User
    participant Client as MCP client
    participant Browser
    participant API as SketchFlow API
    participant DB as Database
    participant Canvas as Open canvas clients

    Client->>API: Discover OAuth metadata and register client
    API-->>Client: OAuth endpoints and client_id
    Client->>API: Start authorization with PKCE challenge and requested scopes
    API-->>Browser: Open authorization page (redirect to sign-in if needed)
    opt User is not signed in
        User->>Browser: Sign in to SketchFlow
        Browser->>API: Complete sign-in and return to authorization
    end
    API-->>Browser: Show requested access and optional write consent
    User->>Browser: Approve read access; optionally approve write access
    Browser->>API: Submit consent
    API->>DB: Store grant and create short-lived authorization code
    API-->>Browser: Redirect with code and state to client callback
    Browser-->>Client: Deliver authorization response
    Client->>API: Exchange code with PKCE verifier
    API->>DB: Validate code and persist access token
    API-->>Client: Return short-lived access token

    loop Each MCP tool call
        Client->>API: Call tool with bearer token and board ID
        API->>DB: Validate token, scope, and current board access
        alt Read tool
            API->>DB: Load authorized boards or board snapshot
            DB-->>API: Return permitted board data
            API-->>Client: Return board data
        else Write tool
            API->>API: Validate input, editor role, revision, and idempotency
            API->>DB: Commit through the canonical revisioned writer
            DB-->>API: Return committed operation and new revision
            API-->>Canvas: Broadcast committed operation
            API-->>Client: Return operation result and new revision
        end
    end
```

## MCP tools

The server implements:

- `list_boards` — list boards the authenticated user may access; support
  pagination and minimal metadata.
- `get_board` — return board metadata, current revision, and a paginated
  element snapshot.
- `draw_shapes` — add typed rectangle, ellipse, or diamond elements.
- `draw_freehand` — add freehand strokes with explicit world-coordinate points.
- `draw_connector` — add lines and arrows between two points.
- `add_text` — add editable text at an explicit position.
- `update_elements` — update an allowlisted set of fields on specified element
  IDs; reject attempts to change identity, ownership, or unsupported fields.
- `delete_elements` — delete specified element IDs after rechecking editor
  permission.

Tool schemas use named fields and bounded arrays rather than accept
arbitrary SQL, URLs, serialized commands, or unrestricted element JSON. Return
the board ID, operation ID, new revision, created/updated/deleted IDs, and
clear conflict or validation information. Avoid an implicit “currently open
board”: external clients should pass a board ID and the backend must authorize
it on every request.

## Safety, reliability, and limits

- Reuse the canonical element schema and collaboration batch validation.
- Enforce per-call payload, element-count, string-length, and coordinate
  bounds; retain existing plan checks used by the shared writer.
- Require an explicit write grant and editor role for mutations. Keep read and
  write OAuth scopes distinct, with revocation available to the user.
- Use revision checks and idempotency keys to make retries safe. On a conflict,
  return the current revision and require the client to reread before retrying;
  never silently overwrite another collaborator's edits.
- Recheck access for every tool call, so removed workspace or board access
  takes effect without waiting for token expiry.
- Use stateless MCP requests, bounded request/tool payloads, and audit logs
  identifying the user/client/tool/board/operation without logging board data
  or access tokens. Rate limiting and operational timeout policy remain
  deployment follow-ups.
- Treat tool outputs as untrusted data for client-side prompt-injection
  handling; do not execute board text, links, or embedded content as
  instructions.
- Keep tool-generated changes attributable to the authorizing user and
  distinguish the MCP client in audit metadata where supported.

## Delivery status

### Backend protocol and authentication — implemented

- RMCP provides the remote Streamable HTTP endpoint, with stateless requests
  and host/origin validation.
- OAuth metadata, dynamic public-client registration, authorization-code with
  PKCE, explicit optional write consent, token revocation, and grant revocation
  are implemented.
- OAuth handlers have focused tests for redirects, scopes, PKCE, and escaping.

### Read-only MCP — implemented

- `list_boards` and paginated `get_board` reuse existing access checks and
  snapshot loading.
- Reads and writes require their respective OAuth scopes; access is rechecked
  for each request.

### Safe canvas writes — implemented; broader integration tests pending

- Typed drawing and element-edit tools use shared schema validation, the
  revisioned writer, current editor-role checks, and collaboration broadcasts.
- Mutations require client-supplied stable operation IDs and base revisions;
  generated IDs/seeds are deterministic for retries.
- Focused unit tests cover element schema, PKCE validation, coordinate limits,
  scopes, and retry identity. Database-backed conflict/access/revocation tests
  and live-client verification remain.

### Client rollout and operations — remaining

- Setup, scopes, endpoint, and grant-revocation documentation is in the
  backend README; compatibility testing with representative MCP clients is
  still needed.
- Rate limiting, latency/failure metrics, feature-flagged rollout, and incident
  response procedures are not part of this backend MVP.

## Acceptance criteria

- At least two supported MCP clients can authenticate and discover the tools
  using the documented remote endpoint.
- An agent can read only boards visible to the authorized user.
- A viewer cannot mutate a board; an editor can create and edit supported
  elements.
- A committed agent drawing appears in an already-open board without a page
  refresh and is persisted with a new revision.
- Retries do not duplicate writes, and concurrent conflicting edits are
  returned as conflicts without data loss.
- Revoking an agent grant or the user's board access prevents subsequent
  access.
- Tool schemas reject malformed, oversized, and unsupported input.

## Confirmed decisions

- Remote Streamable HTTP only; no local stdio adapter in this release.
- OAuth 2.1 authorization-code with PKCE.
- Read permission is requested through OAuth; `boards:write` requires a
  separate explicit consent checkbox.
- Scope is limited to canvas elements: no board creation, comments, or
  presentations.
