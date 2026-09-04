# Proposal: cursor-first pagination (`--limit`/`--continue`)

## Problem

The CLI engine provides one mechanism for dealing with large lists of data, a pagination pattern that uses two flags: `--limit` and `--offset`. This allows a consumer to specify the starting index of their data and how many items they want.

This works OK in the case of manipulations of slicing up in-memory collections, but the other "large lists of data" use case is server-maintained collections. There are three identified patterns of this across our APIs, which do not have consistent pagination mechanisms:

| Pattern | Example Parameters       | 
|---------|--------------------------|
| Slicing | `--offset` + `--limit`   |
| Paging  | `--page` + `--page-size` |
| Cursor  | `--limit` + `--continue` |

✅ If the Slicing pattern is used by an API, the current CLI `--offset` and `--limit` flags translate nicely; we just pass the offset index and limit arguments directly to the API.

⚠️ The Paging pattern is fairly easy to adapt to the Slicing semantics. The main complication is if an offset is asked for that does not align with page boundaries, you would either have to reject that argument or piece together multiple pages from the API then slice the result to match the requested limit.

❌ The Cursor pattern, on the other hand, is a terrible fit for adaptation to Slice-based arguments, especially if there is a server-persisted cursor state. Although it works ok starting at an offset of 0, fetching a second page of data requires starting over from the beginning, iterating over prior data simply to throw it away. There's no defined way to skip ahead. Reading a whole cursor-backed list sequentially costs O(N²) page fetches, not O(N). This is not an edge case; it is the common "give me the next batch" path for this exact backend shape.

So all three patterns _can_ be adapted, but because some of our most key APIs are using the Cursor pattern, and those same APIs are also heavily rate limited, this adaptation is egregious. We should rethink how pagination works in the CLI as a consequence.

## Goal

The goal of this proposal is that we have a single, documented, consistent mechanism for pagination in our CLIs. If pagination works consistently across all commands regardless of if it's in-memory or API based, regardless of API style, then agents and other users don't have to look up and understand how to get all their data on a command-by-command basis.

## Assumptions

- Users and agents typically either don't care about a displayed dataset or want to receive a full collection of data
- Paging exists in CLI output to avoid the cost (like token consumption) of reading data that may not be needed
- Paging exists in APIs to reduce per-request resource consumption and to similarly short-circuit data fetching in case consumers don't actually need the whole thing

## Proposal

My proposal is that we offer an alternative baseline pagination pattern in the CLI engine then switch the `gddy` CLI to this new pattern across the board. I believe the Cursor pattern is actually a better fit.

The front-end cursor pattern used by the CLI involves two pieces: a per-page limit (this could either be user-configurable or an API-constrained/defaulted value), passed with a `--limit` argument, and a continuation token, passed with a `--continue` argument.

The continuation token could be one of two things:

| Token Type   | Example            | Description |
|--------------|--------------------|-------------|
| Handle       | `x83fd20`          | An opaque value, understood by the API, used to recall a suspended stream of data. When the API is asked to continue iteration, it knows what to resume |
| Instructions | `page:2,limit:100` | An instruction-bearing string that can be translated into specific parameters for calling the backing API |

Thus, for a command that is using API-managed continuation tokens, the sequence may look like this:

```
gddy domain list --limit 100
gddy domain list --continue x83fd20
gddy domain list --continue 8ae2wyu
```

For commands with, say, a fixed-page-size scheme on the API, the sequence may look like this:

```
gddy email list --limit 20
gddy email list --continue limit:20,page:2
gddy email list --continue limit:20,page:3
```

Any command implementing cursor-based pagination uses a wrapping envelope, like we do for paginated results, that communicates whether there is more data, the total number of items, the remaining number of items (if we know that from the API response), and the continuation token if we haven't reached the end of iteration. Additionally, the suggested next steps will include the exact full command for fetching the next page of results.

This shared pattern should be flexible to accommodate any back-end scheme while not degrading to horrible performance in other cases.

However, there is an important caveat. With Slicing- or Paging-based APIs that work off a naturally-sorted data set (like by domain name), binary searches are possible; this is not possible with a cursor-based linear iteration approach. This concern could be waved away with this justification:

- An API ought to support _searching_ or _filtering_ to help users find the specific objects that they need so that client-conducted exploration is less necessary.
- A command _could_ support optional additional arguments that work directly with the API's paging options, supplementing the _standard_ cursor arguments.
- LLMs may be clever enough to recognize that Instruction-like continuation tokens can be "hacked" to get exactly to where you'd like to go (skipping pages, etc).

## What Would Change

**Two layers, two different change profiles:**

- **`cli_engine` (the framework):** additive, non-breaking. `CommandSpec::with_pagination` / `PaginationConfig` / `--limit`+`--offset` are untouched and remain fully supported. `CommandSpec::with_cursor` / `CursorConfig` / `--limit`+`--continue` is added as a new, parallel, first-class option. The framework doesn't take a position on which pattern any given consumer CLI should prefer — it offers both, matched to whatever a specific backend's own capability actually is.
- **`gddy` (the consumer CLI, this repo's `cli` app):** breaking, by policy choice. `gddy` adopts `.with_cursor` **universally**, across every one of its currently-paginated commands. Some breaking changes may be able to be avoided if we offer per-command flags as optional deviations from the standard pattern, like `--offset` or `--page`, providing direct alternatives to pre-canned continuation tokens. However, we should avoid supporting faked support for `--offset` arguments so we can avoid inefficient API request patterns.

### Flags

- `--limit <N>` — maximum items to return this invocation. Same meaning as `.with_pagination`'s today; configured per-command via a `default_limit`/`max_limit` pair, driven by whatever the backend actually allows.
- `--continue <TOKEN>` — optional. Omitted means "start from the beginning." An opaque string; a command's adapter defines what's inside it.

### `CommandSpec::with_cursor`

```rust
CommandSpec::new("list", "List things").with_cursor(CursorConfig {
    default_limit: 25,
    max_limit: 500,
})
```

Registers `--limit`/`--continue` the same way `with_pagination` registers
`--limit`/`--offset` — opt-in per command, absent from `--help` and rejected as unknown otherwise. 

### Envelope changes

Today's `PaginationMeta { total, offset, limit, count, has_more }` assumes both `total` and `offset` are always known — true for a client-side slice, not guaranteed for a real cursor API that may never report a true count. The `.with_cursor` counterpart needs `total`/`offset` to become optional (present when an adapter can supply them, absent for a pure opaque cursor) and add `continue_from: Option<String>`.

Human output changes correspondingly when a total is unknown: "Showing 25 items so far — run with `--continue <token>` for more" instead of "Showing 25 of 143 rows, offset 0, limit 25". When a total *is* available (some cursor backends do report one, and every client-side-slice command still knows its own total), the existing "N of M" phrasing still applies.

`next_actions` needs no new mechanism — it already replays every flag the user passed and appends an updated pagination flag; for a `.with_cursor` command it appends `--continue <token>`.

**6 production `gddy` commands lose `--offset` outright**, in the same release:

  - `dns list`
  - `api_explorer response`
  - `api_explorer search`
  - `api_explorer parameter`
  - `application list`
  - `actions_catalog`

Any script, alias, or muscle-memory usage of `--offset` against these commands breaks immediately upon upgrade.

## Prior art

- **AWS CLI**'s `--starting-token`/`--max-items`/`--page-size` is the closest existing precedent for a plain resume token plus a result cap, distinct from the underlying service's own `NextToken`/`Marker`/`ContinuationToken` pagination styles (which differ per AWS service, much like the backend shapes surveyed here). Notably, AWS CLI does not expose a universal arbitrary-offset flag either.
- **`gh`** and **`kubectl`** mostly avoid exposing offset/page for list commands at all, leaning on `--limit` plus internal, invisible cursor-following — a stronger form of this same instinct.
- **GraphQL Relay-style cursor connections** and **Stripe's `starting_after`/`ending_before`** are the same pattern in API design rather than CLI design: opaque cursor plus a count cap, no arbitrary-offset primitive.
