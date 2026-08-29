# Task and issue manager survey

Verified 2026-08-15 against official product and API documentation. This is a compatibility reference, not an implementation roadmap.

## Current target set

| System | Structural model | Important compatibility details |
| --- | --- | --- |
| Trello | Workspace -> board -> list -> card -> checklist item | Collections are labels used to group boards, not a nested project hierarchy. Lists often represent workflow, but users also use them as arbitrary groups. |
| Todoist | Workspace -> project -> section -> task tree | Personal projects nest three indent levels. Team projects use flat, non-nestable folders. Sections are flat within a project. Tasks support deeper nesting than Cadet should artificially cap. |
| Linear | Workspace -> initiative graph -> project -> milestone -> issue tree | Initiatives nest up to five levels and may have multiple parents. Projects themselves are flat. Milestones group issues within projects. |
| GitHub Issues | Organization/repository -> issue tree; Projects collect issues across repositories | Sub-issues form a hierarchy. Projects are flexible views/collections rather than issue containers. Repository identity remains significant. |
| Beads | Repository-scoped issue graph with parent-child, blocking, related, and discovered-from edges | No separate project field in the documented issue model. Parent-child structure and labels provide most grouping. Blocking dependencies are distinct from hierarchy. |

These systems justify keeping four Cadet concepts separate:

- **Project:** storage, synchronization, configuration, and key namespace.
- **Group:** optional single navigation grouping within a project.
- **Parent:** task decomposition and outcome hierarchy.
- **Dependency:** readiness/blocking relationship, independent of parentage.

Tags remain many-to-many cross-cutting classification.

## Strong additions

### Jira

Jira covers configurable enterprise and developer workflows. Its default team-managed Kanban setup is relatively small, while custom fields and statuses still map well to Cadet. The main complications are per-work-type workflows and transition behavior. See [Jira reference](jira.md).

### GitLab Work Items

GitLab complements GitHub and covers self-hosted installations. Groups and subgroups create a project hierarchy; Work Items provide parent-child relationships. GitLab is migrating older Epic APIs toward the Work Item model, so an implementation should target the current Work Item GraphQL schema rather than build on the deprecated Epic surface.

### Asana

Asana adds a general team-work audience. Projects contain sections and tasks; tasks have subtasks and may belong to multiple projects. Multi-homing means a single Cadet project/group field cannot reproduce every Asana placement. Asana documents webhook delivery as at-most-once and recommends periodic polling when missing an event is unacceptable.

## Migration source, not connector

### Taskwarrior

Cadet already covers Taskwarrior's relevant task model: configurable fields, tags, priorities, dates, project organization, dependencies, and local operation. Running Cadet on top of Taskwarrior would add another state owner without a clear benefit.

A one-time importer remains useful. Taskwarrior explicitly supports JSON export/import for third-party applications and warns integrations not to read its private database.

## Later or demand-driven candidates

| System | Assessment |
| --- | --- |
| ClickUp | Capable API and a deep Workspace/Space/Folder/List/task model, but substantial overlap with Jira, Asana, and Trello. |
| Microsoft Planner / To Do | Large ecosystem, but separate Graph models. Planner's API exposes basic plans but not premium plans. |
| Notion | Strong API and webhooks, but no canonical task schema. A connector would require user-configured database-property mappings. |
| monday.com | Configurable board model and up to five subitem layers on multi-level boards. Its broad column model creates significant mapping and versioning work. |
| Azure DevOps | Legitimate enterprise developer target, but overlaps Jira and GitLab and brings area/iteration paths plus work-item link semantics. |
| YouTrack | Complete REST API and good developer fit, but a smaller incremental audience after Jira, Linear, and GitLab. |
| Shortcut | Clean story/epic/iteration API and webhooks, but closely overlaps Linear. |
| Org mode and todo.txt | Better treated as file import/export formats than synchronized remote systems. |

Things, OmniFocus, Apple Reminders, and similar platform-specific tools should wait for a concrete demand and a suitable supported synchronization API.

## Design pressure on Cadet

No common external model preserves every system losslessly:

- Hierarchy depth limits differ.
- Some project structures are trees, some are flat, and Linear initiatives can form a graph.
- Asana permits multiple project memberships; Cadet currently proposes one project and at most one group per task.
- Some systems conflate workflow columns with navigation groups.
- Provider fields may be scalar, reference-valued, calculated, or app-defined.

Cadet should keep an expressive internal task model and make connector limitations explicit. A connector should discover capabilities, preserve stable provider IDs, reject silent flattening, and report which fields or relationships cannot round-trip.

## Sources

- [Trello board collections](https://support.atlassian.com/trello/docs/creating-collections-for-premium-workspaces)
- [Todoist personal subprojects](https://www.todoist.com/help/articles/create-a-sub-project-in-todoist-aTA15C70)
- [Todoist team folders](https://www.todoist.com/help/articles/use-folders-to-organize-team-projects-uoElGQdbb)
- [Todoist sections](https://www.todoist.com/help/articles/introduction-to-sections-rOrK0aEn)
- [Linear initiatives](https://linear.app/docs/sub-initiatives)
- [Linear projects](https://linear.app/docs/projects)
- [Linear milestones](https://linear.app/docs/project-milestones)
- [GitHub sub-issues](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/adding-sub-issues)
- [GitHub sub-issue REST API](https://docs.github.com/en/rest/issues/sub-issues)
- [Beads issue model](https://github.com/gastownhall/beads/blob/d1e725d9f35ba307518551b4e61b3d504fb41ec5/docs/core-concepts/issues.md)
- [GitLab Work Item migration guide](https://docs.gitlab.com/api/graphql/epic_work_items_api_migration_guide/)
- [Asana webhooks](https://developers.asana.com/docs/webhooks-guide)
- [Taskwarrior integration guidance](https://taskwarrior.org/docs/3rd-party/)
- [ClickUp webhooks](https://developer.clickup.com/docs/webhooks)
- [Microsoft Planner API](https://learn.microsoft.com/en-us/graph/api/resources/planner-overview?view=graph-rest-1.0)
- [Notion webhooks](https://developers.notion.com/reference/webhooks)
- [monday.com multi-level boards](https://developer.monday.com/api-reference/docs/working-with-multi-level-boards)
- [YouTrack REST API](https://www.jetbrains.com/help/youtrack/devportal/youtrack-rest-api.html)
- [Shortcut webhooks](https://developer.shortcut.com/api/webhook/v1)
