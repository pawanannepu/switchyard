//! Error codes: the standard JSON-RPC ones plus app codes in the reserved
//! server range (-32000 to -32099), blocked out by area.
//!
//! | Block            | Area     |
//! |------------------|----------|
//! | -32000 .. -32009 | session  |
//! | -32010 .. -32019 | engine   |
//! | -32020 .. -32029 | project  |
//! | -32030 .. -32039 | task     |
//! | -32040 .. -32049 | run      |
//! | -32050 .. -32089 | reserved |
//! | -32090 .. -32099 | host (app host and dev bridge, never the engine) |

macro_rules! error_codes {
    ($($(#[$doc:meta])* $name:ident = $code:literal,)*) => {
        /// Every error code the protocol uses.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $($(#[$doc])* $name,)*
        }

        impl ErrorCode {
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$name,)*];

            pub const fn code(self) -> i32 {
                match self { $(ErrorCode::$name => $code,)* }
            }

            pub const fn name(self) -> &'static str {
                match self { $(ErrorCode::$name => stringify!($name),)* }
            }

            pub fn from_code(code: i32) -> Option<Self> {
                match code { $($code => Some(ErrorCode::$name),)* _ => None }
            }
        }
    };
}

error_codes! {
    // Standard JSON-RPC
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,

    // session
    /// A non stable method was called before a successful `session.hello`.
    HelloRequired = -32000,
    /// `session.hello` was answered, but the protocol majors differ.
    IncompatibleProtocol = -32001,

    // engine
    /// The engine is draining and refuses new work.
    EngineDraining = -32010,
    /// The engine is shutting down.
    EngineShuttingDown = -32011,

    // run
    RunNotFound = -32040,
    /// `run.input` payload is over 64 KiB after decoding.
    InputTooLarge = -32041,
    /// The run is not accepting input right now (its input queue is full).
    InputBusy = -32042,

    // host
    /// The app host (or dev bridge) has no engine connection right now.
    EngineUnavailable = -32090,
    /// The request was in flight when the page reloaded or the engine connection dropped.
    RequestAbandoned = -32091,
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    #[test]
    fn app_codes_should_sit_in_the_reserved_server_range() {
        let is_standard = |code: i32| code == -32700 || (-32603..=-32600).contains(&code);
        let is_app = |code: i32| (-32099..=-32000).contains(&code);
        let out_of_range: Vec<_> =
            ErrorCode::ALL.iter().filter(|c| !is_standard(c.code()) && !is_app(c.code())).collect();
        assert!(out_of_range.is_empty(), "{out_of_range:?}");
    }

    #[test]
    fn from_code_should_invert_code() {
        assert!(ErrorCode::ALL.iter().all(|c| ErrorCode::from_code(c.code()) == Some(*c)));
    }
}
