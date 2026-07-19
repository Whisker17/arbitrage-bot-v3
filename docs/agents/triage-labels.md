# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to
the actual label strings used in this repo's issue tracker (Linear labels).

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the
corresponding label string from this table.

In Linear, these are **issue labels**. Find existing labels with
`linear.list_issue_labels` and create any missing one with `linear.create_issue_label`
before applying it via `linear.save_issue`. Note that lifecycle (open → done) is tracked
separately by Linear **workflow states** — these labels layer the triage role on top of
whatever state the issue is in.

Edit the right-hand column to match whatever vocabulary you actually use.
