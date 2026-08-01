mod anthropic;
mod azure_responses;
mod bedrock;
mod codex;
mod common;
mod faux;
mod google;
mod mistral;
mod openai;
mod pi_messages;
mod radius;
mod responses;
mod vertex;

pub use anthropic::*;
pub use azure_responses::*;
pub use bedrock::*;
pub use codex::*;
pub use faux::*;
pub use google::*;
pub use mistral::*;
pub use openai::*;
pub use pi_messages::*;
pub use radius::*;
pub use responses::*;
pub use vertex::*;

use std::sync::Once;
static BUILTINS: Once = Once::new();
pub fn register_builtins() {
    BUILTINS.call_once(|| {
        register_default_faux();
        register_anthropic();
        register_google();
        register_openai_completions();
        register_openai_codex_responses();
        register_openai_responses();
        register_mistral();
        register_azure_openai_responses();
        register_bedrock();
        register_google_vertex();
        register_pi_messages();
    });
}
