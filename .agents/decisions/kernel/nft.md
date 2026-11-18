# nftables (kernel/nft) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/nft"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on kernel/nft made before it was split from kernel/netns keep their `kernel/netns` tags and live in `.agents/decisions/kernel/netns.md`: D4, D5, D8–D11 and the nftables half of D13.

## Active

### D1: nftables rulesets and evaluation are separate from packet paths

- **What**: kernel/nft owns nft.rs control-plane batches, rulesets and expression evaluation, while packet paths and hook selection remain in kernel/netns.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b
