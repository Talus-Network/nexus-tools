# Hosting options for off-chain tools

## v1 default: GCP Cloud Run

This skill targets **Google Cloud Run** for v1 deployment. Rationale:

- Talus's own `tool-communication.md` recommends conventional cloud (Caddy /
  ALB / managed TLS). No first-party Talus-hosted service exists.
- The Nexus trust model relies on Ed25519 signed HTTP + economic staking,
  not on host decentralization. A "more decentralized" host buys almost no
  trust benefit today.
- Cloud Run gives managed TLS, scale-to-zero, Workload Identity for
  secrets, and Cloud Logging out of the box. Lowest friction for shipping.

The `templates/deploy/` folder emits:

- A multi-stage `Dockerfile` (Rust builder → distroless runtime, port 8080).
- `cloud-run.testnet.yaml` and `cloud-run.mainnet.yaml` (one Cloud Run
  service per env, secret refs, allowed-leaders config).
- `register.sh` (idempotent `nexus tool register` / update).
- Two GitHub Actions workflows (testnet on push to `main`, mainnet on tag).

## Alternatives (per-tool migration targets)

| Provider | Fit | Trade-off |
| --- | --- | --- |
| **Akash Network** | Best general DePIN fit. Mature compute marketplace, supports arbitrary Docker HTTPS services. | More ops friction than Cloud Run; debugging tooling weaker. |
| **Spheron** | Web3 cloud aggregator on top of AWS / Akash. Closest to managed-deploy UX with a DePIN backend. | Newer; smaller ecosystem than Cloud Run. |
| **Atoma Network** | Sui-native AI inference DePIN. Best fit when the tool itself is LLM inference. | Not general-purpose hosting — inference workloads only. |
| **Walrus** | Sui-native blob storage DePIN. Orthogonal — use for tool artifacts, response cache, model weights. | Not a service-hosting platform. |
| **Fluence / io.net / Aethir / Render** | Niche compute DePINs. | P2P or GPU-specific; overkill for typical API wrappers. |
| **AWS Cloud Run-equivalent / Fly.io / Render.com** | Drop-in alternatives if the team prefers another vendor. | Same trade-offs as Cloud Run; no Web3 angle. |

## Migration path

The `Dockerfile` the skill emits is portable. To move a single tool to
Akash or Spheron later:

1. Push the existing image to a public registry (Docker Hub / GHCR).
1. Write a small SDL (Akash) or Spheron config that references the image.
1. Run `nexus tool register offchain --tool-fqn <fqn> --url <new-url>` to
   point Nexus at the new URL. Idempotent — the FQN stays the same.

No Rust code changes required.

## When to revisit this decision

- **Cost.** If Cloud Run egress / minimum instances become material at
  scale, Akash is meaningfully cheaper for steady-state workloads.
- **Narrative.** If the team is marketing Talus as a fully decentralized
  stack, hosting tools on GCP undermines the story. Pick a flagship tool
  (the one most-mentioned in talks) and deploy it to Akash as the
  publicly-visible example.
- **First-party LLM tools.** When Talus ships its own LLM-inference tool
  crate, deploy it to Atoma — the workload is the DePIN service.
