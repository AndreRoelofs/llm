//! Chat wrapper that adds memory capabilities to any ChatProvider.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::{
    chat::{ChatMessage, ChatProvider, ChatResponse, Tool},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    memory::{MemoryProvider, MessageCondition},
    stt::SpeechToTextProvider,
    tts::TextToSpeechProvider,
    LLMProvider,
};

/// Adds transparent long-term memory to any `ChatProvider`.
pub struct ChatWithMemory {
    provider: Arc<dyn LLMProvider>,
    memory: Arc<RwLock<Box<dyn MemoryProvider>>>,
    role: Option<String>,
    role_triggers: Vec<(String, MessageCondition)>,
}

impl ChatWithMemory {
    /// Build a new wrapper.
    ///
    /// * `provider` – Underlying LLM backend
    /// * `memory` – Conversation memory store
    /// * `role` – Optional agent role
    /// * `role_triggers` – Reactive rules
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        memory: Arc<RwLock<Box<dyn MemoryProvider>>>,
        role: Option<String>,
        role_triggers: Vec<(String, MessageCondition)>,
    ) -> Self {
        let wrapper = Self {
            provider,
            memory: memory.clone(),
            role,
            role_triggers: role_triggers.clone(),
        };

        if !wrapper.role_triggers.is_empty() {
            wrapper.spawn_reactive_listener();
        }

        wrapper
    }

    /// Spawn a background listener that reacts to memory events.
    fn spawn_reactive_listener(&self) {
        let memory = self.memory.clone();
        let provider = self.provider.clone();
        let role_triggers = self.role_triggers.clone();
        let my_role = self.role.clone();

        tokio::spawn(async move {
            let mut receiver = {
                let guard = memory.read().await;
                match guard.get_event_receiver() {
                    Some(r) => r,
                    None => return,
                }
            };

            while let Ok(event) = receiver.recv().await {
                if !role_triggers
                    .iter()
                    .any(|(role, cond)| role == &event.role && cond.matches(&event))
                {
                    continue;
                }

                let context = {
                    let guard = memory.read().await;
                    guard.recall("", None).await.unwrap_or_default()
                };

                let response_text = match provider.chat(&context).await {
                    Ok(r) => r.text().map(|s| s.to_string()),
                    Err(e) => {
                        eprintln!("Reactive chat error: {e}");
                        None
                    }
                };

                if let (Some(txt), Some(role)) = (response_text, &my_role) {
                    let formatted = format!("[{role}] {txt}");
                    let msg = ChatMessage::assistant().content(formatted).build();
                    let mut guard = memory.write().await;
                    if let Err(e) = guard.remember_with_role(&msg, role.clone()).await {
                        eprintln!("Memory save error: {e}");
                    }
                }
            }
        });
    }

    /// Access the wrapped provider.
    pub fn inner(&self) -> &dyn LLMProvider {
        self.provider.as_ref()
    }

    /// Dump the full memory (debugging).
    pub async fn memory_contents(&self) -> Vec<ChatMessage> {
        let guard = self.memory.read().await;
        guard.recall("", None).await.unwrap_or_default()
    }
}

#[async_trait]
impl ChatProvider for ChatWithMemory {
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        {
            // record incoming user messages once
            let mut mem = self.memory.write().await;
            for m in messages {
                let mut tagged = m.clone();
                if let Some(role) = &self.role {
                    tagged.content = format!("[{role}] User: {}", m.content);
                    mem.remember_with_role(&tagged, role.clone()).await?;
                } else {
                    mem.remember(&tagged).await?;
                }
            }
        }

        // build context
        let mut context = {
            let mem = self.memory.read().await;
            mem.recall("", None).await?
        };

        // summarization if needed
        let needs_summary = {
            let mem = self.memory.read().await;
            mem.needs_summary()
        };
        if needs_summary {
            let summary = self.provider.summarize_history(&context).await?;
            let mut mem = self.memory.write().await;
            mem.replace_with_summary(summary);
            context = mem.recall("", None).await?;
        }

        context.extend_from_slice(messages);
        let response = self.provider.chat_with_tools(&context, tools).await?;

        // record assistant reply once
        if let Some(text) = response.text() {
            let memory = self.memory.clone();
            let tag = self.role.clone();
            tokio::spawn(async move {
                let mut mem = memory.write().await;
                let formatted = match &tag {
                    Some(r) => format!("[{r}] {text}"),
                    None => text.to_string(),
                };
                let msg = ChatMessage::assistant().content(formatted).build();
                let res = if let Some(r) = tag {
                    mem.remember_with_role(&msg, r).await
                } else {
                    mem.remember(&msg).await
                };
                if let Err(e) = res {
                    eprintln!("Memory save error: {e}");
                }
            });
        }

        Ok(response)
    }

    async fn memory_contents(&self) -> Option<Vec<ChatMessage>> {
        let guard = self.memory.read().await;
        Some(guard.recall("", None).await.unwrap_or_default())
    }
}

#[async_trait]
impl CompletionProvider for ChatWithMemory {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.provider.complete(req).await
    }
}

#[async_trait]
impl EmbeddingProvider for ChatWithMemory {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        self.provider.embed(input).await
    }
}

#[async_trait]
impl SpeechToTextProvider for ChatWithMemory {
    async fn transcribe(&self, audio: Vec<u8>) -> Result<String, LLMError> {
        self.provider.transcribe(audio).await
    }
}

#[async_trait]
impl TextToSpeechProvider for ChatWithMemory {
    async fn speech(&self, text: &str) -> Result<Vec<u8>, LLMError> {
        self.provider.speech(text).await
    }
}

impl LLMProvider for ChatWithMemory {
    fn tools(&self) -> Option<&[Tool]> {
        self.provider.tools()
    }
}
