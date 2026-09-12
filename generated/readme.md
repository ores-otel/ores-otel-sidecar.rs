# Generated code — do not edit

Everything in this directory is **derivative output**. It is produced by a
generator from an upstream source of truth (TypeSpec and/or JSON Schema, an
ORM schema, or a protocol definition), and it is regenerated wholesale.

**Any edit you make here will be silently destroyed the next time the
generator runs.** If something in this directory is wrong, the defect is in
the generator or in the upstream schema — fix it there.

## Why the files are read-only

`scripts/lock-generated.sh` strips the write bit from every file in this tree
so an accidental save fails loudly instead of being lost later.

Note that git records only the executable bit, not the read-only bit, so the
permissions do **not** survive a fresh clone. Re-run the script after cloning,
and wire it into the generator step and CI so the guarantee is enforced rather
than merely documented.

## Regenerating

Run the generator for this repository, then re-run `scripts/lock-generated.sh`.
