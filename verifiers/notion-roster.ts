// verifiers/notion-roster.ts   caps: { net: ["api.notion.com"] }
//
// A source GENERATOR, not a verifier: it authenticates, fetches, and normalizes
// a source into a ground value, which it prints as JSON on stdout. ken hashes
// that value and a pure predicate judges it (e.g. `--predicate ptr:/owner:equals`).
// The generator does no judging of its own; it only produces the bytes to read.
//
// KEN_VALUE is the claim (here, the page id whose roster we want). Capabilities
// are declared and folded into the content hash, so adding `net` shows up as a
// loud diff and a `ken grant`.
const pageId = Deno.env.get("KEN_VALUE")!;
const res = await fetch(`https://api.notion.com/v1/blocks/${pageId}/children`);
const body = await res.json();

// Normalize to just the fields the fact cares about, like a `jq` pass.
const roster = (body.results ?? []).map((b: { id: string; type: string }) => ({
  id: b.id,
  type: b.type,
}));
console.log(JSON.stringify({ owner: body.owner ?? null, roster }));
