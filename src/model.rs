#[derive(Clone)]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub preview: String,
    pub unread: i32,
    pub broadcast: bool,
}

#[derive(Clone)]
pub struct Message {
    pub id: i32,
    pub sender: String,
    pub timestamp: i64,
    pub text: String,
    pub outgoing: bool,
    pub attachment: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct UpdateStatus {
    pub revision: u64,
    pub reconnecting: bool,
}
