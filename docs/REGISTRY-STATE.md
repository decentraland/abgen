# Registry conversion state

`registry_records` stores Registry conversion state and the routing keys needed
to select converted entities. Catalyst remains authoritative for entity bodies,
content files and deployers. Registration resolves each entity through the
configured `EntitySource`; this table does not store a second entity document.

The workload is an administrative batch of entity IDs, deduplicated in request
order, followed by keyed refreshes and pointer-overlap reads within a world or
Genesis City. Each distinct imported entity adds one row. Payloads contain
bundle status and version documents, an entity timestamp and its pointer array.
Read/write rates, production cardinality and row sizes have not been measured.

`entity_id` is the primary key. `world_name`, `entity_timestamp` and `pointers`
are projections of the immutable Catalyst entity obtained at registration;
world names and pointers are normalized to lowercase. Empty `world_name` denotes
Genesis City. There is no cross-schema foreign key because entity availability
and conversion retention have different lifecycles; a read resolves the body
again through Catalyst and reports unavailable registered bodies as upstream errors. Re-registering an ID
refreshes its conversion facts and routing projection.

Registration takes sorted transaction advisory locks for the entity ID and
world/pointer keys before refreshing conversion manifests. This orders observations
with record publication so a waiting refresh cannot commit an older failure over
a newer success. It reads related non-obsolete rows and commits the record and fallback
rotation together. A newer complete or fallback record makes an older import
obsolete. Otherwise mac/windows asset results determine complete, failed or
pending. The newest older complete/fallback record becomes fallback while the
new import is pending/failed, or obsolete when the import is complete/obsolete.
Older pending records remain pending so their later successful conversion can
still become available if a newer conversion fails.

Evicting derived manifest files does not erase a known conversion result. Under
the same transaction locks, observations preserve existing complete or failed
platform facts, including versions and dates, even if another runtime retains an
older failed artifact. Only explicit conversion completion updates the attempted
platform; it cannot overwrite other platforms from stale local observations. Artifact delivery can regenerate evicted bytes independently
of these durable conversion facts.

Active pointer reads return complete/fallback records in descending entity
timestamp order and optionally exclude Registry-denylisted IDs. Exact record
reads include pending, failed and obsolete records. The table has only its
primary-key index; pointer-overlap and case-insensitive lookup plans, latency,
storage and write costs require measurements before adding secondary indexes.
These are functional queries, not a production performance claim.

The additive `crate/migrations/0004_registry_records.sql` migration belongs to
abgen's existing SQLx migration ledger and registry database search path. It
creates the table on fresh or existing abgen databases without resetting data.
Registration states survive process restarts. Importing records from the
authoritative local entity source and current conversion manifests rebuilds the
projection; fallback history requires retaining the relevant entity IDs and
their conversion results.

## Database contract checks

The PostgreSQL integration tests are ignored by the default unit-test run.
Set `CATALYST_TEST_DATABASE_URL` to a disposable PostgreSQL database, then run:

```sh
cargo test --locked -p abgen --lib registry:: -- --include-ignored --test-threads=1
cargo test --locked -p abgen --lib abcdn::handlers::entities -- --include-ignored --test-threads=1
```

Each test creates a uniquely named schema and drops it after its assertions.
The registration test applies migrations 0001–0003, inserts an existing moderation
row, applies 0004–0005, checks registration/fallback transitions and evicted-manifest
handling, then reconnects and reruns migrations. The HTTP handler tests cover
active/version responses, denylist filtering, worlds isolation and undeployment.
These functional checks do not establish production query performance.

`registry_conversion_attempts` stores one monotonically increasing generation per
entity/platform conversion lane. HTTP JIT and durable workers reserve a generation
before starting work; completing an older generation cannot replace newer facts.
The workload is one keyed upsert per attempted conversion and one locked lookup at
completion. Its composite primary key serves both operations; no secondary indexes
or duplicated entity bodies are added. Migration 0005 creates this coordination
state additively. A worker completion validates its current queue claim and writes
platform facts plus terminal job status in the same transaction. A superseded
generation retires its still-owned job receipt as failed; the claim-token check
prevents that retirement from changing a receipt already requeued or reclaimed.

Durable queue workers use a private converter instance, content/bundle cache and
output directory with Space disabled. They publish only fenced database facts;
private files are removed after the blocking conversion and its background fetches
finish. A canceled or superseded worker cannot publish served artifacts. This
intentionally does not warm the public artifact cache: HTTP JIT regenerates bytes
when requested, which may repeat conversion work and add cold-request latency.
HTTP JIT reserves the same generation before conversion, so a delayed HTTP result
cannot overwrite a newer worker's durable facts. HTTP artifact delivery retains
its existing filesystem/Space behavior; the generation transaction governs
Registry facts and job acknowledgement, not a distributed artifact transaction.
