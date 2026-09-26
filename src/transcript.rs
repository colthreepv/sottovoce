//! STUB — owned by the transcript builder. Turns words and speaker turns of
//! both sides into one conversation.

use crate::types::{Meeting, SideInput, Speaker, Utterance};

/// Builds speakers and paragraphs on the meeting timeline from both sides.
pub fn build(_sides: &[SideInput], _your_name: Option<&str>) -> (Vec<Speaker>, Vec<Utterance>) {
    (Vec::new(), Vec::new())
}

/// transcript.md for a meeting.
pub fn to_markdown(meeting: &Meeting) -> String {
    format!("# {}\n", meeting.title)
}
