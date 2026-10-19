# Datasets

An Ark keeps its data in slots, one per kind of dataset. The personal slots
hold the owner's variant calls, which ark uploads with their approval on the
phone. A public sample can fill all three instead, fetched with the same
approvals. The reference slots hold public data that the Ark offers for
download, which ark fetches without an approval. Every data command but
`ark data samples` needs a paired, unlocked Ark, as `ark help devices`
describes. `ark data paths` maps what apps read from the slots, as
`ark help apps` describes.

## Slots

`ark data list` shows every slot and its state, and `ark data show SLOT`
shows one slot in full. A command names a slot as the list prints it, or by
its number, which keeps a slot this build has no name for usable. An Ark
has these slots:

- reference-genome, the human reference genome assembly
- gene-annotations, the genes and their coordinates on that assembly
- small-variants, the owner's SNVs and short indels, from a VCF file
- structural-variants, the owner's structural variants, such as deletions,
  duplications, inversions and breakends, from a VCF file
- copy-number-variants, the owner's copy number segments, from a VCF file
- variant-catalog, dbSNP's catalog of rsIDs and their positions

With --json each slot carries:

- slot and id, its name and number
- name, description and format, the Ark's own texts. description says what
  the slot holds. format says which file fills it, the shape that file
  needs, what the Ark refuses and whether the owner approves the upload.
- state, which is empty, filled or damaged. A damaged slot holds files whose
  metadata is missing, corrupt or outdated, and damage says what is wrong.
- origin, which is personal or reference
- requires, the slots that must be filled before this one accepts data
- size_bytes, the space the slot takes on the Ark's disk, 0 when empty
- build, the assembly the data is on, such as GRCh38.p14, or for an empty
  reference slot the assembly of the file it offers
- version, the dbSNP release of the variant catalog, such as 157, and null
  for the other slots
- download, the file the Ark offers for a reference slot that is not
  filled, with url, size_bytes and sha256, otherwise null

`ark data show` adds required_by, the slots that require this one, and
cached, whether this computer's download cache holds the offered file. Its
reading view marks each requirement as filled or not filled. The reading
view of `ark data list` shows slot, state, build, version and size, and
hints at the command that fills or erases a slot.

The Ark offers the references for the assembly family of the owner's calls,
so GRCh38.p14 references beside GRCh38.p13 calls are normal.

## Filling an Ark

Every other slot requires small-variants. The Ark offers the references for
the assembly of the owner's small variants, and takes structural variants
and copy numbers only on that assembly family. Calls on GRCh37 or GRCh38 get
all three references, and calls on other assemblies, such as T2T-CHM13v2.0,
only some. A fresh Ark fills in three steps:

1. `ark data upload calls.vcf.gz` uploads the owner's small variants,
   approved on their phone. `ark data fetch --sample NAME` instead fills all
   three personal slots from one public sample, each file approved the same
   way.
2. `ark data fetch --all` downloads every reference the Ark offers for the
   calls' assembly, without an approval.
3. `ark data upload` takes the structural variant and copy number files from
   the same sequencing, each approved on the phone. Both are optional, and a
   sample has filled them already.

## Uploading

`ark data upload FILE` uploads one of the owner's variant call files, a VCF
file with one person's genotypes, plain or compressed. The Ark first
identifies the file from its first bytes, which picks its slot among
small-variants, structural-variants and copy-number-variants. It refuses any
other kind of file with file-rejected, naming the reason, and
`ark data show SLOT` gives the exact format each slot accepts. --slot names
the slot you expect, and the upload fails with invalid-slot when the Ark
identifies another. --dry-run stops after the identification and reports
the target slot's state and requirements.

The target slot has to be empty, since the Ark refuses to overwrite a
dataset, and the slots it requires have to be filled. So structural variants
and copy numbers fail with dependency-missing while small-variants is empty,
before any approval. Replacing the owner's small variants takes
`ark data delete small-variants` first, which leaves the slots requiring it
in place. Delete the structural variants and copy numbers too, since they
come from the same sequencing, and the references when the new calls are on
another assembly family. The owner approves the upload on their phone within
1 minute, and ark uploads the file and waits while the Ark validates and
indexes it, which takes minutes to an hour. A file that fails validation
ends with error[ark], exit 5, naming the reason. Once processing has started
it runs to the end on the Ark, even when ark is interrupted.

## Reference downloads

`ark data fetch SLOT` downloads the file the Ark offers for a reference slot
that is not filled, and streams it to the Ark. ark checks the bytes against
the offered SHA-256 on the way, and a mismatch stops the upload before the
Ark processes it. `ark data fetch --all` does the same for every reference
slot that is not filled, in the order their requirements set, and skips the
filled ones. ark never builds a reference's download address itself.

--all skips a reference slot that offers no file, as for calls on an
assembly without that reference, and notes which. It fails with
dependency-missing while small-variants is empty, and checks every offer
before it downloads anything. Naming a slot that offers no file fails
with no-download. The first failure stops the rest. Each slot ends as done,
skipped, failed or not-attempted, or as planned or skipped under --dry-run.
--json returns them as fetched, each with slot, id, url, size_bytes, sha256,
cached, outcome and error, and a sample's row adds sample and name.

Downloads go through a cache on this computer, whose location `ark doctor`
shows. A file is kept under its SHA-256, and a cached file uploads again
without a download. An interrupted download keeps what arrived and resumes
from there, and network failures get 3 attempts in all. --cache picks
another directory. --no-cache streams a reference without keeping a copy,
and keeps each sample file in a temporary file until its upload ends. The
cache holds only public files, references and samples.

## Public samples

`ark data samples` lists public genomes for testing an Ark with real data.
They are the 1000 Genomes Project's 3,202 people, each with small variant,
structural variant and copy number calls made by Illumina's DRAGEN 4.4.7.
The list is built into ark and needs no Ark.

Each sample keeps a fixed name made from its sample ID, an adjective and a
given name that matches the person's sex, such as affable-hazel for HG03730.
The name never changes, so a test, a bug report and a colleague all reach
the same genome. Commands take the name or the ID. Any two adjectives, and
any two given names, differ by at least two edits, counting two swapped
neighboring letters as one. So one wrong, missing, extra or swapped letter
fails instead of reaching another person.

With no population the command lists the 26 populations, each with its
code, its number of people and a description. A code lists that
population's people in sample ID order, --sex narrows them, and --all lists
all 3,202.

--json returns populations, each with population, count and description.
With a population or --all it returns samples, each with name, sample,
population, sex, father and mother, where a parent outside the cohort is
null.

`ark data fetch --sample NAME` fills the three personal slots from one
sample, named or given by its sample ID, so a sample never mixes with other
data. While any personal slot holds data, damaged included, it fails with
slot-filled before it downloads anything. It then fills small-variants,
structural-variants and copy-number-variants in that order. For each file,
ark downloads the whole file and checks it against a size and SHA-256 built
into ark. Only then does the upload ask the owner on their phone, as for
their own calls, and the Ark processes the file like any upload. The first
failure stops the rest. A failed upload is not retried, since that would ask
the owner again, but the cached copies save the downloads on the next run.
A sample's three files take 410 to 570 MiB, nearly all of it the small
variants.

## Emptying a slot

`ark data delete SLOT` empties a slot that holds a dataset, and refuses an
empty one. `ark data repair SLOT` erases a slot whatever its state, which
recovers a damaged or half-written slot that delete refuses, and erases a
healthy dataset too. Both wait up to 2 minutes for the owner's approval on
the phone. Their --dry-run shows the slot's state and the slots that
require it, and changes nothing.
