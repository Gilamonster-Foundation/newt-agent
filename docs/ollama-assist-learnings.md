# OllamAssist Learnings for Newt-Agent

Findings from analyzing OllamAssist's architecture in the context of enhancing Newt-Agent. These are transferable patterns that could strengthen Newt-Agent's capabilities.

## 1. RAG-based workspace awareness → proactive context retrieval

**What OllamAssist does:** Before generating a response, it scans the workspace to surface relevant files and includes them in its reply. This is an *active* search for context rather than relying solely on what was already provided.

**Why Newt-Agent should care:** Newt-Agent has explicit truncation studies showing that silent truncation produces silently wrong answers. However, it currently doesn't have a proactive "find what's relevant" step before starting work — it relies on the tiered router (Fast/Standard/Complex/Review) to decide depth.

**How to apply this:** Combine OllamAssist's RAG approach with Newt-Agent's tiered router:
- Before routing to Complex or Review tiers, do a workspace scan so the agent knows *what* it needs to read rather than guessing from truncated context.
- This creates a "context discovery" phase that happens before execution begins.

**Implementation note:** The RAG scanning should be lightweight — just surface file paths and relevance scores, then let Newt-Agent's existing routing decide how deep to dive into each file.

## 2. Conversation persistence per project → session continuity across reboots

**What OllamAssist does:** Saves conversations as JSON with configurable history search, allowing recovery of past sessions even after restarts.

**Why Newt-Agent should care:** Newt-Agent's conversation store is critical for maintaining context across agent turns, but it needs to verify how long sessions survive a process restart — that's the gap worth checking. If Newt-Agent drops context on restart without a clean recovery path, OllamAssist's persistence model is directly applicable.

**How to apply this:** Implement a conversation store with:
- JSON-backed storage per project (matching OllamAssist's approach)
- Configurable session duration and history limits
- Recovery mechanism that reconstructs context from persisted sessions on restart

**Implementation note:** This should be opt-in for long-running agents, not the default — most tasks complete within a single session.

## 3. Context-aware commit messages → better audit trail

**What OllamAssist does:** Generates commits with what changed + why + related issues/docs visible in its reply immediately after generation. It includes the commit message inline so users see it without leaving the chat interface.

**Why Newt-Agent should care:** Newt-Agent already produces detailed commit messages from instructions (one file per commit with WHAT/WHY explanations), but doesn't surface them as part of the context stream during active work — only at commit time.

**How to apply this:** Surface recent commits as part of the context stream:
- After each commit, add a brief summary to the agent's working memory
- Allow agents to reference "what I just did" without needing to re-read the full commit log
- This creates continuity between tasks that are related through their git history

**Implementation note:** Keep this lightweight — just store the last N commits (configurable default: 10) in a compact JSON format.

## 4. Sliding window memory → token budget awareness

**What OllamAssist does:** Implements a LangChain4j-style sliding window with configurable size (default 25 messages), explicitly telling users what context was dropped and what remained.

**Why Newt-Agent should care:** Newt-Agent's truncation studies already show that silent truncation is dangerous — the difference is OllamAssist *admits* to dropping context and tells you what it kept. That transparency about "I have X tokens left, here's what I'm keeping" could make Newt-Agent more honest with users about its limitations.

**How to apply this:** Add a visible token budget indicator:
- Show remaining tokens as the conversation grows
- Explicitly state when context is being dropped and what was preserved
- Allow configuration of how much history to keep vs. discard

**Implementation note:** This should be displayed in the agent's output stream so users see exactly what happened with their context window — matching OllamAssist's approach of transparency over opacity.

## Summary Table

| Feature | OllamAssist Pattern | Newt-Agent Application | Priority |
|---------|---------------------|----------------------|----------|
| RAG workspace scan | Scan before answering | Pre-routing context discovery | High |
| Session persistence | JSON-backed, configurable duration | Recovery on restart with history limits | Medium |
| Commit message surfacing | Inline in reply | Working memory after commit | Low |
| Token budget transparency | Explicit drop notices | Visible token indicators + honesty | High |

## Feature Roadmap

Based on the learnings above, here is a concrete feature roadmap for implementing these patterns in newt-agent:

### Phase 1: Workspace-Aware Context Discovery (High Priority)
- **Goal:** Before routing to Complex/Review tiers, scan workspace to surface relevant files
- **Implementation:** Lightweight RAG-based file indexing that prioritizes by naming conventions, modification timestamps, and dependency references
- **Benefit:** Reduces unnecessary context loading; agent knows *what* to read rather than guessing

### Phase 2: Conversation History Persistence (Medium Priority)
- **Goal:** JSON-backed storage per project with configurable session duration and history limits
- **Implementation:** Store conversations alongside state files, implement recovery mechanism on restart
- **Benefit:** Session continuity across reboots; learning from previous interactions within the same project

### Phase 3: Context-Aware Commit Messages (Low Priority)
- **Goal:** Surface recent commits as part of context stream after each commit
- **Implementation:** Store last N commits in compact JSON format, add to working memory
- **Benefit:** Better audit trail; continuity between tasks related through git history

### Phase 4: Token Budget Transparency (High Priority)
- **Goal:** Visible token indicators showing remaining budget and explicit drop notices
- **Implementation:** Display remaining tokens as conversation grows; state when context is dropped and what was preserved
- **Benefit:** Honest transparency about limitations; users see exactly what happened with their context window

### Success Metrics
1. Context Relevance: Percentage of detected relevant files actually used in task
2. Conversation Utilization: How often previous conversations inform new decisions  
3. Commit Quality: Subjective assessment of commit message descriptiveness and adherence to conventions
4. User Satisfaction: Feedback on how these features improve developer experience

### Implementation Timeline
- **Months 1-2:** Foundation (conversation storage schema, basic workspace indexing, state integration)
- **Months 3-4:** Enhancement (RAG-based relevance detection, context-aware commits, optimization for large codebases)
- **Months 5-6:** Refinement (algorithm tuning based on usage data, summarization for long histories, documentation)

## TL;DR

OllamAssist is reactive (responds to chat), so its patterns are simpler than what an autonomous agent needs. But the *intent* — "before I answer, figure out what context actually matters" — applies directly to Newt-Agent. The most high-leverage thing would be combining OllamAssist's workspace-awareness with Newt-Agent's tiered router: scan first, then route and execute.
