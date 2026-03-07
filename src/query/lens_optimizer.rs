use crate::query::lens_resolver::{
    NestMatchCondition, NestMatchOp, ResolvedAggregationNode,
};
use crate::query::{ResolvedNode, ResolvedOperand};

/// Performs various optimizations on the resolved AST.
pub fn optimize(node: ResolvedNode) -> ResolvedNode {
    match node {
        ResolvedNode::And(children) => {
            let mut optimized_children: Vec<ResolvedNode> =
                children.into_iter().map(optimize).collect();
            merge_nest_matches(&mut optimized_children, false);
            if optimized_children.len() == 1 {
                optimized_children.pop().unwrap()
            } else {
                ResolvedNode::And(optimized_children)
            }
        }
        ResolvedNode::Or(children) => {
            let mut optimized_children: Vec<ResolvedNode> =
                children.into_iter().map(optimize).collect();
            // DuckDB has a bug evaluating `HAVING A OR B` when A or B contains an `IN` subquery with `INTERSECT`.
            // By NOT merging ORs, it falls back to UNION which works perfectly.
            // merge_nest_matches(&mut optimized_children, true);
            if optimized_children.len() == 1 {
                optimized_children.pop().unwrap()
            } else {
                ResolvedNode::Or(optimized_children)
            }
        }
        ResolvedNode::Difference(l, r) => ResolvedNode::Difference(
            Box::new(optimize(*l)),
            Box::new(optimize(*r)),
        ),
        ResolvedNode::Complement(c) => {
            ResolvedNode::Complement(Box::new(optimize(*c)))
        }
        ResolvedNode::NestNestMatch {
            left_operand,
            left_nvalue,
            left_context,
            op,
            right_operand,
            right_nvalue,
            right_context,
        } if left_operand == right_operand
            && left_context == right_context
            && matches!(op, NestMatchOp::Comparison(_)) =>
        {
            // Convert self-comparing NestNestMatch into a MergedNestMatch
            ResolvedNode::MergedNestMatch {
                operand: left_operand,
                matches: vec![NestMatchCondition {
                    nvalue: left_nvalue,
                    op,
                    right: right_nvalue,
                    context: left_context,
                }],
                is_or: false,
            }
        }
        ResolvedNode::AggregationMatch { agg, op, label } => {
            ResolvedNode::AggregationMatch {
                agg: flatten_aggregation(agg),
                op,
                label,
            }
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            ResolvedNode::AggregationCalculationMatch {
                agg: flatten_aggregation(agg),
                op,
                calc,
            }
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            ResolvedNode::AggregationAggregationMatch {
                left: flatten_aggregation(left),
                op,
                right: flatten_aggregation(right),
            }
        }
        ResolvedNode::AggregationTagMatch {
            agg,
            op,
            tag_type,
            storage,
            sql_type,
        } => ResolvedNode::AggregationTagMatch {
            agg: flatten_aggregation(agg),
            op,
            tag_type,
            storage,
            sql_type,
        },
        ResolvedNode::NestMatch {
            operand,
            nvalue,
            op,
            label,
            context,
        } if nvalue.contains_aggregation() => ResolvedNode::MergedNestMatch {
            operand,
            matches: vec![NestMatchCondition {
                nvalue,
                op: NestMatchOp::Comparison(op),
                right: ResolvedOperand::Literal(label),
                context,
            }],
            is_or: false,
        },
        _ => node,
    }
}

// (Moved import to the top)

fn flatten_aggregation(
    agg: ResolvedAggregationNode,
) -> ResolvedAggregationNode {
    use crate::query::ast::ArithmeticAggOp::*;
    use crate::query::lens_resolver::ResolvedAggregationNode::Arithmetic;
    use crate::query::ResolvedNode::Nest;
    use crate::query::ResolvedOperand::Aggregation;

    // 1. Check if outer is an arithmetic aggregation
    let Arithmetic { op: outer, inner } = &agg else {
        return agg;
    };

    // 2. Check if inner is a projection without context
    let Nest {
        nvalue: Some(nval),
        context: None,
        ..
    } = &**inner
    else {
        return agg;
    };

    // 3. Check if the value is another aggregation (specifically arithmetic)
    let Aggregation(inner_agg) = nval else {
        return agg;
    };
    let Arithmetic {
        op: inner_op,
        inner: inner_inner,
    } = inner_agg
    else {
        return agg;
    };

    // 4. Verify they are combinable (Sum+Sum, Max+Max, Min+Min)
    let is_combinable =
        matches!((outer, inner_op), (Sum, Sum) | (Max, Max) | (Min, Min));
    if !is_combinable {
        return agg;
    }

    Arithmetic {
        op: *outer,
        inner: inner_inner.clone(),
    }
}

fn merge_nest_matches(children: &mut Vec<ResolvedNode>, is_or: bool) {
    // We will group NestMatch nodes by (operand, context)
    // To use them as hash keys, we could derive Hash or just use a O(N^2) search since N is usually small.
    // Let's use a simple O(N^2) grouping approach to avoid requiring Hash trait on AST.
    let mut groups: Vec<(
        crate::query::ResolvedOperand,
        Option<Box<ResolvedNode>>,
        Vec<NestMatchCondition>,
    )> = Vec::new();

    let mut remaining = Vec::new();

    for child in children.drain(..) {
        match child {
            ResolvedNode::NestMatch {
                operand,
                nvalue,
                op,
                label,
                context,
            } => {
                let cond = NestMatchCondition {
                    nvalue,
                    op: NestMatchOp::Comparison(op),
                    right: crate::query::ResolvedOperand::Literal(label),
                    context: context.clone(),
                };

                let mut found = false;
                for group in &mut groups {
                    if group.0 == operand && group.1 == context {
                        group.2.push(cond.clone());
                        found = true;
                        break;
                    }
                }

                if !found {
                    groups.push((operand, context, vec![cond]));
                }
            }
            ResolvedNode::MergedNestMatch {
                operand,
                matches,
                is_or: child_is_or,
            } if child_is_or == is_or || matches.len() == 1 => {
                // すでにマージされているが、同じレベル (AND/OR) かつ operand が同じならさらにマージ可能
                // MergedNestMatch の各 condition は自身の context を持つが、
                let mut found = false;
                // Contextが同じ場合のみマージ可能
                let matches_context =
                    matches.first().and_then(|m| m.context.clone());
                for group in &mut groups {
                    if group.0 == operand && group.1 == matches_context {
                        group.2.extend(matches.clone());
                        found = true;
                        break;
                    }
                }

                if !found {
                    groups.push((operand, None, matches));
                }
            }
            _ => remaining.push(child),
        }
    }

    // Now reconstruct the children vector
    for (operand, _context, matches) in groups {
        if matches.len() > 1 {
            remaining.push(ResolvedNode::MergedNestMatch {
                operand,
                matches,
                is_or,
            });
        } else {
            // Just one match, convert back to NestMatch to avoid unnecessary merging
            let cond = matches
                .into_iter()
                .next()
                .expect("matches should have at least one element");
            let NestMatchOp::Comparison(op) = cond.op else {
                panic!(
                    "Expected ComparisonOp for single NestMatch, got: {:?}",
                    cond.op
                );
            };
            let crate::query::ResolvedOperand::Literal(label) = cond.right
            else {
                panic!(
                    "Expected Literal for single NestMatch, got: {:?}",
                    cond.right
                );
            };
            remaining.push(ResolvedNode::NestMatch {
                operand,
                nvalue: cond.nvalue,
                op,
                label,
                context: cond.context,
            });
        }
    }

    *children = remaining;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ResolvedOperand;

    use crate::query::lens_resolver::Resolver;

    #[test]
    fn test_optimize_same_key_merge_logical() {
        let query_str = "parentdir: &: (count(ext:rs) > 0) & parentdir: &: (sum(size:) > 1000)";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;

        // At this point `resolved` is an `And` node containing two NestMatches.
        let optimized = optimize(resolved);

        match optimized {
            ResolvedNode::MergedNestMatch {
                operand,
                matches,
                is_or,
            } => {
                assert!(!is_or, "Should be an AND merge");
                assert_eq!(matches.len(), 2, "Should have 2 merged conditions");

                // operand (GROUP BY x) が parentdir であることを確認
                match operand {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "parentdir");
                    }
                    _ => panic!(
                        "Expected TagRef(parentdir) as operand, got: {:?}",
                        operand
                    ),
                }
            }
            _ => {
                panic!("Expected MergedNestMatch as root, got: {:?}", optimized)
            }
        }
    }

    #[test]
    fn test_optimize_same_key_merge_or() {
        let query_str = "parentdir: &: (count(ext:rs) > 0) | parentdir: &: (sum(size:) > 1000)";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;

        let optimized = optimize(resolved);

        match optimized {
            ResolvedNode::Or(children) => {
                assert_eq!(
                    children.len(),
                    2,
                    "Should remain an Or node with 2 children"
                );
                for child in children {
                    match child {
                        ResolvedNode::MergedNestMatch {
                            operand,
                            matches,
                            is_or,
                        } => {
                            assert!(!is_or, "Child projection match should not be OR");
                            assert_eq!(matches.len(), 1, "Child should have 1 condition");
                            match operand {
                                ResolvedOperand::TagRef { tag_type, .. } => {
                                    assert_eq!(tag_type.as_str(), "parentdir");
                                }
                                _ => panic!("Expected TagRef(parentdir) as operand, got: {:?}", operand),
                            }
                        }
                        _ => panic!("Expected children to be MergedNestMatch, got: {:?}", child),
                    }
                }
            }
            _ => panic!("Expected Or as root, got: {:?}", optimized),
        }
    }

    #[test]
    fn test_optimize_filter_pushdown() {
        let query_str = "parentdir: &: (count(*:*) > 0) & type:file";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        let ResolvedNode::And(nodes) = optimized else {
            panic!("Expected And node, got: {:?}", optimized);
        };

        let proj_match = nodes
            .into_iter()
            .find(|n| matches!(n, ResolvedNode::NestMatch { .. }))
            .expect("Should contain a NestMatch");

        let ResolvedNode::NestMatch {
            context: Some(ctx), ..
        } = proj_match
        else {
            panic!("Context should be populated with type:file");
        };

        fn is_type_file(n: &ResolvedNode) -> bool {
            match n {
                ResolvedNode::ColumnMatch { tag, label } => {
                    tag.as_str() == "type" && label.as_str() == "file"
                }
                ResolvedNode::Match {
                    tag_type, label, ..
                } => tag_type.as_str() == "type" && label.as_str() == "file",
                ResolvedNode::And(nodes) => nodes.iter().any(is_type_file),
                _ => false,
            }
        }

        assert!(
            is_type_file(&ctx),
            "Context should contain type:file: {:?}",
            ctx
        );
    }

    #[test]
    fn test_optimize_flatten_sum_sum() {
        // Query: sum(parentdir: &: sum(size:)) > 0
        // Outer sum should be flattened to essentially sum(size:) > 0.
        let query_str = "sum(parentdir: &: sum(size:)) > 0";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        // Compare with explicitly flat sum(size:) > 0
        let flat_query_str = "sum(size:) > 0";
        let flat_resolved =
            Resolver::new(flat_query_str).unwrap().resolved_query;

        match (optimized, flat_resolved) {
            (
                ResolvedNode::AggregationMatch { agg: opt_agg, .. },
                ResolvedNode::AggregationMatch { agg: flat_agg, .. },
            ) => {
                assert_eq!(opt_agg, flat_agg, "The optimized double sum should exactly match the single flat sum inner aggregation");
            }
            (o, f) => panic!(
                "Expected both to be AggregationMatch, got {:?} and {:?}",
                o, f
            ),
        }
    }

    #[test]
    fn test_optimize_same_key_merge_comparison() {
        // ((parentdir: &: count(size:))) := ((parentdir: &: sum(size:)))
        // 同一キー (parentdir) の NestNestMatch → MergedNestMatch に変換されること
        let query_str =
            "((parentdir: &: count(size:))) := ((parentdir: &: sum(size:)))";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        match optimized {
            ResolvedNode::MergedNestMatch {
                operand,
                matches,
                is_or,
            } => {
                assert!(!is_or, "Comparison merge should be AND (is_or=false)");
                assert_eq!(
                    matches.len(),
                    1,
                    "Single comparison should produce 1 merged condition"
                );
                match operand {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "parentdir");
                    }
                    _ => panic!(
                        "Expected TagRef(parentdir) as operand, got: {:?}",
                        operand
                    ),
                }
                // op は Comparison であること
                assert!(
                    matches!(matches[0].op, NestMatchOp::Comparison(_)),
                    "Condition op should be Comparison, got: {:?}",
                    matches[0].op
                );
            }
            _ => panic!("Expected MergedNestMatch, got: {:?}", optimized),
        }
    }

    #[test]
    fn test_optimize_same_key_merge_arithmetic() {
        // (parentdir: &: count(ext:rs)) / (parentdir: &: count()) :> 100
        // 同一キーの算術演算 nvalue → Calculation を持つ MergedNestMatch に変換されること
        let query_str =
            "((parentdir: &: count(ext:rs)) / (parentdir: &: count())) :> 100";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        match optimized {
            ResolvedNode::MergedNestMatch {
                operand,
                matches,
                is_or,
            } => {
                assert!(!is_or, "Should be AND (is_or=false)");
                assert_eq!(matches.len(), 1, "Should have 1 merged condition");
                match operand {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "parentdir");
                    }
                    _ => {
                        panic!("Expected TagRef(parentdir), got: {:?}", operand)
                    }
                }
                // nvalue は Calculation (count / count の算術式) であること
                assert!(
                    matches!(
                        matches[0].nvalue,
                        ResolvedOperand::Calculation(_)
                    ),
                    "nvalue should be Calculation, got: {:?}",
                    matches[0].nvalue
                );
            }
            _ => panic!("Expected MergedNestMatch, got: {:?}", optimized),
        }
    }

    #[test]
    fn test_optimize_different_key_no_merge() {
        // parentdir と extension は異なるキーなのでマージされないこと
        let query_str =
            "parentdir: &: (count(ext:rs) > 0) & extension: &: (count(*:*) > 0)";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        // MergedNestMatch にはなってはいけない
        assert!(
            !matches!(optimized, ResolvedNode::MergedNestMatch { .. }),
            "Different keys should NOT be merged into MergedNestMatch"
        );
        // And として維持されること
        assert!(
            matches!(optimized, ResolvedNode::And(_)),
            "Different keys should remain as And, got: {:?}",
            optimized
        );
    }

    #[test]
    fn test_optimize_different_simple_key_no_merge() {
        // extension: の And ラッパーなしのシンプルな異なるキー同士はマージされないこと
        let query_str =
            "parentdir: &: (sum(size:) > 0) & stem: &: (count(*:*) > 0)";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        assert!(
            !matches!(optimized, ResolvedNode::MergedNestMatch { .. }),
            "Different simple keys (parentdir vs stem) should NOT be merged"
        );
        assert!(
            matches!(optimized, ResolvedNode::And(_)),
            "Should remain as And, got: {:?}",
            optimized
        );
    }

    #[test]
    fn test_optimize_no_flatten_avg_avg() {
        // avg(parentdir: &: avg(size:)) は sum と違いフラット化されないこと
        let query_str = "avg(parentdir: &: avg(size:)) > 0";
        let resolved = Resolver::new(query_str).unwrap().resolved_query;
        let optimized = optimize(resolved);

        // 比較対象: avg(size:) > 0 (フラット版)
        let flat_resolved =
            Resolver::new("avg(size:) > 0").unwrap().resolved_query;

        match (optimized, flat_resolved) {
            (
                ResolvedNode::AggregationMatch { agg: opt_agg, .. },
                ResolvedNode::AggregationMatch { agg: flat_agg, .. },
            ) => {
                assert_ne!(
                    opt_agg, flat_agg,
                    "avg(avg(...)) should NOT be flattened (unlike sum)"
                );
            }
            (o, f) => panic!(
                "Expected both to be AggregationMatch, got {:?} and {:?}",
                o, f
            ),
        }
    }
}
