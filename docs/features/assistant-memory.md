# Long-term memory

On Web and Tauri, open **AI Assistant → Multi-device conversation**. On Android, select **Personal conversation** from the device list menu. On iOS, select it in the device list. You can chat without selecting a device. With automatic memory enabled, the assistant can save and retrieve your personal memories. Open **History** to resume, start or delete a personal conversation.

On Web and Tauri, reopening the assistant restores the conversation last selected in that browser. A failed restore shows **Retry** and **New conversation**. Sending resumes after a successful restore or an explicit new conversation; starting a new conversation preserves the old stored data.

You can submit separate requests while the assistant is busy. Pending messages stay with the original conversation. The current request finishes its approvals and background follow-up before another request starts. Reopen the conversation to see the queue. Expand the pending messages above the composer to cancel an individual request. After stopping the conversation, select **Resume queue** to process the remaining requests.

Conversations saved in an unsupported format remain in History, marked “Cannot continue this conversation,” with deletion available after confirmation. They follow the same idle-session retention period. Work still in flight, unknown outcomes and unsettled usage keep their required evidence under the existing rules.

The conversation list uses short titles. During the first exchange, the assistant uses the selected model to name the conversation from your first message. A short excerpt appears while the title is being generated or if generation fails. Title generation counts toward normal model usage.

## Personalization

On Web and Tauri, open **+ → Manage → Personalization** in a conversation, or use the AI Assistant overview. On Android, use the device-list menu; on iOS, open it from the device list.

Enter instructions for new conversations, such as your preferred language, answer order and level of detail. The default capacity is 8 KiB; the current service configuration determines the limit. Saved changes apply to new conversations. Existing conversations keep their initial settings, and scheduled tasks keep the personalization saved with their definitions.

The same page has separate **Automatic memory** and **Cross-conversation search** switches. Existing memories remain available to review and delete when automatic memory is off; cross-conversation search can be enabled independently. If settings changed elsewhere, the page keeps your draft and asks you to reload. Reloading replaces unsaved edits.

## Review and delete long-term memories

Long-term memory serves the personal AI Assistant and belongs to your account across devices and models. The assistant extracts stable facts during normal chat and can handle explicit requests to save, correct or forget a fact. Describe what you need in the conversation; use the management page to review or delete entries.

Ask “What did I previously ask you to call me?” The assistant normally uses current memories and can read retained, accessible revisions for historical questions. A version’s recording time is when the platform saved it; check its sources for the actual event time. Forgetting a memory also blocks new reads of its old revisions.

On Web, open **Long-term memories** from the AI Assistant overview. On Android, use the device-list menu; on iOS, open it from the device list. Filter by personal or device scope and inspect the text, background, version, recorded and updated times, and available sources.

- **Delete one:** select **Delete memory** and confirm. If the entry changed, review its latest version before confirming again.
- **Clear all:** confirm separately to remove all your personal and device memories, including entries outside the current filter.
- **Correct a fact:** explain the correction in chat so the assistant can update it from the available evidence.

Deletion stops using the fact as current memory and prevents learning it again from its known old evidence. Original chats and personalization remain available; delete the relevant conversation to remove its text. If several facts share an indivisible source excerpt, deleting one may also affect access to that excerpt for the others.

Use the server-confirmed result and the management page to check deletion. An uncertain network result can be retried. The assistant continues after receiving the tool result; a failed follow-up answer leaves a committed deletion in effect.

### View sources

Select a source in an answer, **View original conversation** on a memory, or a history result to open read-only history in the AI Assistant chat page. It highlights the original message and lets you browse nearby, earlier and latest content. Retained history remains readable while the source device is offline, and your current draft and task stay in place.

**View latest conversation content** loads the newest page and scrolls to its end. **Locate reference** returns to the quoted message, and **Load earlier messages** pages back in time. Messages appear in chronological order.

After the original chat expires naturally, a memory with valid independent evidence can show that retained evidence separately from the full chat. Manually deleting a conversation withdraws its sources and its child-conversation sources and stops using memories that depend on them. Deleted, expired or inaccessible sources appear unavailable. After forgetting a memory, you can still view its lawfully retained original chat.

Clickable sources come from records verified by the platform. Each answer defaults to at most 64 citations and 8 KiB of citation metadata; both limits are configurable. Incomplete presentation is indicated. Source links follow current access, version and retention rules.

### Saved conversation content

Retrieved content becomes an independent snapshot once saved in the current conversation. Later deletion, expiry or updates to the original chat leave those saved excerpts, answers and summaries available. Deleting the current conversation removes the content it owns. Current memory refreshes for each model request; existing answers retain their original content.

New searches and original source links follow current source availability. Copy, export and hard content-expiry restrictions still apply. Historical records describe past events; another action requires current permissions and target state.

### Saving memories and cost

The conversation model writes the content and context and decides whether to add or update a memory. The platform checks sources, versions and capacity, then commits the write. The assistant uses the tool result to report the outcome. Updates retain accessible historical versions; a version conflict requires a fresh read before another update.

Memory saving uses the normal conversation's model calls and allowance. Your provider's records determine BYOM charges.

## Temporary conversations

Select **New temporary conversation** in the personal AI Assistant conversation list. Temporary conversations use personalization but neither read nor write long-term memory, search other conversations, nor enter cross-conversation search. Ordinary chat records, required audit and task-recovery data follow their existing retention rules; delete the conversation when you need to remove its records.

The mode is fixed when the first input is accepted and survives refreshes, reconnects and retries. Create a new conversation to change modes.

## Searching other conversations

Enable **Cross-conversation search**, then ask, for example, “Find my earlier research on this topic” or “Who did I send files to last week?” The assistant searches your account's accessible retained records in the current deployment, including conversations on other devices, and reads nearby source text when needed. Each search still follows its authorized device, time and content scope. Both fixed-device and multi-device conversations can search: by default only the scope related to the current conversation (the current device, or in a multi-device conversation, conversations without a device plus the selected devices), and on request every device you own, for both conversations and memories.

Search covers user inputs, assistant replies and tool records permitted for retention. It uses keywords and time ranges and can find matches near the end of long messages. Results show an excerpt before the assistant reads more source text. A miss or incomplete search only describes the evidence currently available; a historical statement alone also does not establish that a task finished.

### Relative dates

“Yesterday” and “last week” use the system time zone of the client sending that message. Web/Tauri, Android and iOS send their client zone with each input, and retries retain the original zone. The receipt timestamp records when the platform accepted the input; the text and evidence determine when the event happened. Scheduled tasks use their own time-zone settings.

### Goals and scheduled tasks

Existing goals and scheduled tasks retain their originally authorized memory and history scope. Create a new task to use capabilities enabled later. Turning a switch off restricts later reads. New owner messages in a long-running task follow the automatic-memory setting at send time; earlier inputs sent while it was disabled stay unchanged.

Scheduled requirements and results enter search only when their sources permit retention. Manually entered requirements do not automatically become long-term facts; AI-generated requirements inherit source restrictions. Each run keeps its original requirements when the task is edited later.

## Owner history search and management

Open **History search** in AI Assistant to browse your account's retained content across devices by keywords and time range. Leave keywords empty to browse by time. Owner access is independent of the model's cross-conversation-search switch and works while the source device is offline. Results indicate incomplete coverage when candidate, time or capacity budgets are reached.

**Retained conversations** lists each root conversation once. Conversations whose execution state was reclaimed remain read-only. Confirmed deletion stops remaining work, withdraws associated history and dependent memories, and preserves required accounting and device-file cleanup records under their existing rules. External actions such as emails already sent remain in effect.

## Data processing and technical retries

**Data processing status** shows history coverage, capture gaps and storage counts. Use the reported reasons to assess capture results.

Storage counts may include records awaiting cleanup. Use the reported reason to assess capture gaps or insufficient capacity. The long-term-memory page shows published memories.

<span id="background-processing-policy"></span>

## Content and retrieval policy

Deployment administrators can find capacity, retrieval and retention settings in the [configuration reference](/config/config-toml#assistant-data-policy).
