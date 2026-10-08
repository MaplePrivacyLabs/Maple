use super::compaction_common::*;
use pi_agent_core::types::AgentMessage;
use pi_coding_agent::core::compaction::utils::{
    compute_file_lists, create_file_ops, extract_file_ops_from_message,
};
use serde_json::json as j;

mod compaction_file_operations {
    use super::*;

    #[test]
    fn include_files_touched_by_nested_calls_recorded_on_tool_results() {
        let message: AgentMessage = json(
            j!({"role":"toolResult","toolCallId":"codemode-1","toolName":"codemode",
            "content":[],"isError":false,"timestamp":0,"nestedCalls":{"calls":[
                {"id":"codemode-1/1","name":"read","arguments":{"path":"a.ts"},"status":"ok"},
                {"id":"codemode-1/2","name":"edit","arguments":{"path":"b.ts","edits":[]},"status":"ok"},
                {"id":"codemode-1/3","name":"write","argumentsBytes":40000,"status":"ok"}
            ],"complete":false}}),
        );
        let mut file_ops = create_file_ops();
        extract_file_ops_from_message(&message, &mut file_ops);
        assert_eq!(
            observed(&compute_file_lists(&file_ops)),
            j!({"readFiles":["a.ts"],"modifiedFiles":["b.ts"]})
        );
    }
}
