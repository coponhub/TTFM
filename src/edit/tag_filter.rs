use crate::query::QueryNode;
use anyhow::Result;

pub fn parse_tag_condition(_condition: &str) -> Result<QueryNode> {
    // TODO: TagCondition 評価（別フェーズ）
    todo!("parse_tag_condition")
}

pub fn eval_tag_predicate(_node: &QueryNode, _tagged_at: Option<i64>) -> Result<bool> {
    // TODO: TagCondition 評価（別フェーズ）
    todo!("eval_tag_predicate")
}
