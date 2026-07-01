use pie_agent_core::{AgentMessage, JsonlSessionRepo};

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());
    let session = repo.create("/some/cwd").await.unwrap();
    let msg = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("hello".into()),
        timestamp: 12345,
    }));
    session.append_message(msg).await.unwrap();
    let files = repo.list().await.unwrap();
    let content = std::fs::read_to_string(&files[0]).unwrap();
    println!("{}", content);
}
