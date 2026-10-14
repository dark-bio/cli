# Datasets

An Ark keeps its data in slots, one per kind of dataset. The personal slot
holds the owner's variant calls, which ark uploads with their approval on
the phone, or a public sample's, which it fetches with the same approval.
The reference slots hold public data that the Ark offers for download, which
ark fetches without an approval. Every data command but
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
- snp-indel-calls, the owner's variant calls, from a VCF file
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

Every reference slot requires snp-indel-calls, since the Ark offers the
references for the assembly of the owner's calls. Calls on GRCh37 or GRCh38
get all three references, and calls on other assemblies, such as
T2T-CHM13v2.0, only some. A fresh Ark fills in two steps:

1. `ark data upload calls.vcf.gz` uploads the owner's calls, approved on
   their phone. `ark data fetch snp-indel-calls --sample NAME` fetches a
   public sample instead, approved the same way.
2. `ark data fetch --all` downloads every reference the Ark offers for the
   calls' assembly, without an approval.

## Uploading

`ark data upload FILE` uploads the owner's variant calls, a VCF file with
one person's genotypes, plain or compressed. The Ark first identifies the
file from its first bytes, and refuses any other kind of file with
file-rejected, naming the reason. `ark data show snp-indel-calls` gives the
exact format it accepts. --slot names the slot you expect, and the upload
fails with invalid-slot when the Ark identifies another. --dry-run stops
after the identification and reports the target slot's state and
requirements.

The target slot has to be empty, since the Ark refuses to overwrite a
dataset. Replacing the owner's calls takes `ark data delete snp-indel-calls`
first. New calls on another assembly family leave the references of the old
one in place, so delete those too and fetch them again. The owner approves
the upload on their phone within 1 minute, and ark uploads the file and
waits while the Ark validates and indexes it, which takes minutes to an
hour. A file that fails validation ends with error[ark], exit 5, naming the
reason. Once processing has started it runs to the end on the Ark, even when
ark is interrupted.

## Reference downloads

`ark data fetch SLOT` downloads the file the Ark offers for a reference slot
that is not filled, and streams it to the Ark. ark checks the bytes against
the offered SHA-256 on the way, and a mismatch stops the upload before the
Ark processes it. `ark data fetch --all` does the same for every reference
slot that is not filled, in the order their requirements set, and skips the
filled ones. ark never builds a reference's download address itself.

--all skips a reference slot that offers no file, as for calls on an
assembly without that reference, and notes which. It fails with
dependency-missing when the owner's calls are missing, and checks every
offer before it downloads anything. Naming a slot that offers no file fails
with no-download. The first failure stops the rest. Each slot ends as done,
skipped, failed or not-attempted, or as planned or skipped under --dry-run.
--json returns them as fetched, each with slot, id, url, size_bytes, sha256,
cached, outcome and error, and a sample's row adds sample and name.

Downloads go through a cache on this computer, whose location `ark doctor`
shows. A file is kept under its SHA-256, and a cached file uploads again
without a download. An interrupted download keeps what arrived and resumes
from there, and network failures get 3 attempts in all. --cache picks
another directory. --no-cache streams a reference without keeping a copy,
and keeps a sample in a temporary file until its upload ends. The cache
holds only public files, references and samples.

## Public samples

`ark data samples` lists public genomes for testing an Ark with real data.
They are the variant calls of the 1000 Genomes Project's 3,202 people, made
by Illumina's DRAGEN 4.4.7. The list is built into ark and needs no Ark.

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

`ark data fetch snp-indel-calls --sample NAME` fills the empty personal slot
with a sample, named or given by its sample ID. ark downloads the whole file
first and checks it against a size and SHA-256 built into it. Only then does
the upload ask the owner on their phone, as for their own calls, and the Ark
processes the calls like any upload. A failed upload is not retried, since
that would ask the owner again, but the cached copy saves the download on
the next run. A sample's calls take 390 to 550 MiB.

## Emptying a slot

`ark data delete SLOT` empties a slot that holds a dataset, and refuses an
empty one. `ark data repair SLOT` erases a slot whatever its state, which
recovers a damaged or half-written slot that delete refuses, and erases a
healthy dataset too. Both wait up to 2 minutes for the owner's approval on
the phone. Their --dry-run shows the slot's state and the slots that
require it, and changes nothing.
