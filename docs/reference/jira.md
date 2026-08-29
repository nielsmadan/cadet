# Jira reference

Verified 2026-08-15 against Jira Cloud documentation. This is a product-model reference, not a committed connector scope.

## Fresh team-managed Kanban project

For the team-managed Kanban template considered for Cadet, all work types in a newly created project initially use the same project-local workflow:

```text
To do -> In progress -> Done
```

Jira can later assign different workflows to different work types. That is a customization, not the starting configuration. A connector should discover the live workflow associations instead of assuming the project still matches its template.

Kanban uses `Task` as its default standard work type; Scrum uses `Story`. Jira documents `Epic`, `Story`, `Task`, `Bug`, and `Subtask` as its suggested software work types. Subtasks are enabled by default.

The default hierarchy is:

```text
Epic
└── Story, Task, or Bug
    └── Subtask
```

Subtasks cannot contain further subtasks. Cadet can retain arbitrary hierarchy internally, but a lossless Jira connector must reject or explicitly transform trees deeper than Jira's configured hierarchy.

## Team-managed and company-managed

| | Team-managed | Company-managed |
| --- | --- | --- |
| Administration | Project-local; a space admin can configure it without a Jira admin | Jira-admin-controlled screens, schemes, and workflows |
| Reuse | Configuration belongs to one project | Configuration can be shared across projects |
| Starting complexity | Lower | Potentially much higher |
| Connector consequence | The recommended starting profile | Support should depend on discovered capabilities, not the project label |

Atlassian's current UI documentation increasingly says *space*. The REST API and much existing terminology still use *project*.

Team-managed does not guarantee simplicity. It supports custom statuses, workflow rules, and different workflows by work type. Conversely, a company-managed project may be compatible if its discovered schema is simple enough.

## Cadet representation

Cadet can represent more Jira data than a minimal fixed-field mapping suggests:

- `State` is an arbitrary string.
- A project workflow defines arbitrary states, initial and terminal states, and a static transition graph.
- Custom fields support string, text, integer, float, boolean, date, datetime, enum, and string-list values.
- Field definitions can be required.

The canonical implementations are `crates/core/src/model.rs` and `crates/core/src/config.rs`.

| Jira | Cadet | Notes |
| --- | --- | --- |
| Summary | Title | Direct mapping |
| Description | Body | Jira uses Atlassian Document Format, so conversion is required |
| Status | State | Custom names and static branches are representable |
| Priority | Priority plus an optional exact Jira field | Jira has more priority values than Cadet's canonical three |
| Due date | Due | Jira may require the field to be enabled per work type |
| Labels | Tags | Direct many-valued mapping |
| Components | String-list custom field | Preserve Jira component IDs in connector metadata so renames remain stable |
| Ordinary custom fields | Typed custom fields | Discover type, allowed values, requiredness, and context |
| Parent | Parent task | Subject to Jira's configured hierarchy |

Required custom fields are not inherently unsupported. During setup, the connector can map supported Jira fields into required Cadet fields or collect configured defaults. Creation should fail during preflight only when a required Jira field has no value or uses an unsupported type.

## Boundary of generic support

The difficult features are behavioral or provider-specific rather than merely additional fields:

- separate workflows for different work types when Cadet has one project workflow
- transition conditions, validators, permissions, approvals, and post-functions
- calculated and read-only fields
- user, group, sprint, version, Assets, cascading-select, and Marketplace field types
- administering Jira schemas, workflows, components, and automation

Cadet can mirror a static workflow graph, but it cannot reproduce every contextual Jira transition rule locally. The connector should query the transitions available for the specific Jira work item before writing a state change.

## Recommended compatibility check

Binding should inspect the existing Jira project rather than create or reconfigure it. The check should discover:

1. Project style and product type.
2. Work types and their hierarchy levels.
3. Workflow assignment, statuses, terminal categories, and available transitions.
4. Create/edit fields for every synchronized work type.
5. Required fields, allowed values, components, and unsupported schemas.

The setup command can then generate a proposed Cadet mapping and report any lossy or unsupported parts before synchronization begins.

## Sources

- [Team-managed and company-managed projects](https://support.atlassian.com/jira-software-cloud/docs/what-are-team-managed-and-company-managed-projects/)
- [Team-managed work types](https://support.atlassian.com/jira-software-cloud/docs/set-up-issue-types-in-team-managed-projects/)
- [Team-managed workflows](https://support.atlassian.com/jira-software-cloud/docs/manage-how-work-flows-in-your-team-managed-project/)
- [Team-managed subtasks](https://support.atlassian.com/jira-software-cloud/docs/manage-subtasks-in-team-managed-projects/)
- [Configure the due-date field](https://support.atlassian.com/jira-software-cloud/docs/add-the-due-date-field-to-your-issues/)
- [Jira Cloud issue and transition API](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issues/)
- [Jira Cloud project and status API](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-projects/)
