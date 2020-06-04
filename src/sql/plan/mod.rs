mod optimizer;
mod planner;
use optimizer::Optimizer as _;
use planner::Planner;

use super::engine::Transaction;
use super::execution::{Executor, ResultSet};
use super::parser::ast;
use super::schema::{Catalog, Table};
use super::types::{Expression, Value};
use crate::error::Result;

use serde_derive::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{self, Display};

/// A query plan
#[derive(Debug)]
pub struct Plan(pub Node);

impl Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.to_string())
    }
}

impl Plan {
    /// Builds a plan from an AST statement.
    pub fn build<C: Catalog>(statement: ast::Statement, catalog: &mut C) -> Result<Self> {
        Planner::new(catalog).build(statement)
    }

    /// Executes the plan, consuming it.
    pub fn execute<T: Transaction + 'static>(self, txn: &mut T) -> Result<ResultSet> {
        Executor::build(self.0).execute(txn)
    }

    /// Optimizes the plan, consuming it.
    pub fn optimize<C: Catalog>(self, catalog: &mut C) -> Result<Self> {
        let mut root = self.0;
        root = optimizer::ConstantFolder.optimize(root)?;
        root = optimizer::FilterPushdown.optimize(root)?;
        root = optimizer::IndexLookup::new(catalog).optimize(root)?;
        root = optimizer::NoopCleaner.optimize(root)?;
        Ok(Plan(root))
    }
}

/// A plan node
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Aggregation {
        source: Box<Node>,
        aggregates: Vec<Aggregate>,
    },
    CreateTable {
        schema: Table,
    },
    Delete {
        table: String,
        source: Box<Node>,
    },
    DropTable {
        table: String,
    },
    Filter {
        source: Box<Node>,
        predicate: Expression,
    },
    IndexLookup {
        table: String,
        alias: Option<String>,
        column: String,
        values: Vec<Value>,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        expressions: Vec<Vec<Expression>>,
    },
    KeyLookup {
        table: String,
        alias: Option<String>,
        keys: Vec<Value>,
    },
    Limit {
        source: Box<Node>,
        limit: u64,
    },
    NestedLoopJoin {
        left: Box<Node>,
        left_size: usize,
        right: Box<Node>,
        predicate: Option<Expression>,
        pad: bool,
        flip: bool,
    },
    Nothing,
    Offset {
        source: Box<Node>,
        offset: u64,
    },
    Order {
        source: Box<Node>,
        orders: Vec<(Expression, Direction)>,
    },
    Projection {
        source: Box<Node>,
        expressions: Vec<(Expression, Option<String>)>,
    },
    Scan {
        table: String,
        alias: Option<String>,
        filter: Option<Expression>,
    },
    // Uses BTreeMap for test stability
    Update {
        table: String,
        source: Box<Node>,
        expressions: BTreeMap<String, Expression>,
    },
}

impl Node {
    /// Recursively transforms nodes by applying functions before and after descending.
    pub fn transform<B, A>(mut self, before: &B, after: &A) -> Result<Self>
    where
        B: Fn(Self) -> Result<Self>,
        A: Fn(Self) -> Result<Self>,
    {
        self = before(self)?;
        self = match self {
            n @ Self::CreateTable { .. }
            | n @ Self::DropTable { .. }
            | n @ Self::IndexLookup { .. }
            | n @ Self::Insert { .. }
            | n @ Self::KeyLookup { .. }
            | n @ Self::Nothing
            | n @ Self::Scan { .. } => n,

            Self::Aggregation { source, aggregates } => {
                Self::Aggregation { source: source.transform(before, after)?.into(), aggregates }
            }
            Self::Delete { table, source } => {
                Self::Delete { table, source: source.transform(before, after)?.into() }
            }
            Self::Filter { source, predicate } => {
                Self::Filter { source: source.transform(before, after)?.into(), predicate }
            }
            Self::Limit { source, limit } => {
                Self::Limit { source: source.transform(before, after)?.into(), limit }
            }
            Self::NestedLoopJoin { left, left_size, right, predicate, pad, flip } => {
                Self::NestedLoopJoin {
                    left: left.transform(before, after)?.into(),
                    left_size,
                    right: right.transform(before, after)?.into(),
                    predicate,
                    pad,
                    flip,
                }
            }
            Self::Offset { source, offset } => {
                Self::Offset { source: source.transform(before, after)?.into(), offset }
            }
            Self::Order { source, orders } => {
                Self::Order { source: source.transform(before, after)?.into(), orders }
            }
            Self::Projection { source, expressions } => {
                Self::Projection { source: source.transform(before, after)?.into(), expressions }
            }
            Self::Update { table, source, expressions } => {
                Self::Update { table, source: source.transform(before, after)?.into(), expressions }
            }
        };
        after(self)
    }

    /// Transforms all expressions in a node by calling .transform() on them with the given closure.
    pub fn transform_expressions<B, A>(self, before: &B, after: &A) -> Result<Self>
    where
        B: Fn(Expression) -> Result<Expression>,
        A: Fn(Expression) -> Result<Expression>,
    {
        Ok(match self {
            n @ Self::Aggregation { .. }
            | n @ Self::CreateTable { .. }
            | n @ Self::Delete { .. }
            | n @ Self::DropTable { .. }
            | n @ Self::IndexLookup { .. }
            | n @ Self::KeyLookup { .. }
            | n @ Self::Limit { .. }
            | n @ Self::NestedLoopJoin { .. }
            | n @ Self::Nothing
            | n @ Self::Offset { .. }
            | n @ Self::Scan { filter: None, .. } => n,

            Self::Filter { source, predicate } => {
                Self::Filter { source, predicate: predicate.transform(before, after)? }
            }
            Self::Insert { table, columns, expressions } => Self::Insert {
                table,
                columns,
                expressions: expressions
                    .into_iter()
                    .map(|exprs| exprs.into_iter().map(|e| e.transform(before, after)).collect())
                    .collect::<Result<_>>()?,
            },
            Self::Order { source, orders } => Self::Order {
                source,
                orders: orders
                    .into_iter()
                    .map(|(e, o)| e.transform(before, after).map(|e| (e, o)))
                    .collect::<Result<_>>()?,
            },
            Self::Projection { source, expressions } => Self::Projection {
                source,
                expressions: expressions
                    .into_iter()
                    .map(|(e, l)| Ok((e.transform(before, after)?, l)))
                    .collect::<Result<_>>()?,
            },
            Self::Scan { table, alias, filter: Some(filter) } => {
                Self::Scan { table, alias, filter: Some(filter.transform(before, after)?) }
            }
            Self::Update { table, source, expressions } => Self::Update {
                table,
                source,
                expressions: expressions
                    .into_iter()
                    .map(|(k, e)| e.transform(before, after).map(|e| (k, e)))
                    .collect::<Result<_>>()?,
            },
        })
    }

    // Displays the node, where prefix gives the node prefix.
    pub fn format(&self, mut indent: String, root: bool, last: bool) -> String {
        let mut s = indent.clone();
        if !last {
            s += "├─ ";
            indent += "│  "
        } else if !root {
            s += "└─ ";
            indent += "   ";
        }
        match self {
            Self::Aggregation { source, aggregates } => {
                s += &format!(
                    "Aggregation: {}\n",
                    aggregates.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                );
                s += &source.format(indent, false, true);
            }
            Self::CreateTable { schema } => {
                s += &format!("CreateTable: {}\n", schema.name);
            }
            Self::Delete { source, table } => {
                s += &format!("Delete: {}\n", table);
                s += &source.format(indent, false, true);
            }
            Self::DropTable { table } => {
                s += &format!("DropTable: {}\n", table);
            }
            Self::Filter { source, predicate } => {
                s += &format!("Filter: {}\n", predicate);
                s += &source.format(indent, false, true);
            }
            Self::IndexLookup { table, column, alias: _, values } => {
                s += &format!("IndexLookup: {}.{}", table, column);
                if !values.is_empty() && values.len() < 10 {
                    s += &format!(
                        " ({})",
                        values.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")
                    );
                } else {
                    s += &format!(" ({} values)", values.len());
                }
                s += "\n";
            }
            Self::Insert { table, columns: _, expressions } => {
                s += &format!("Insert: {} ({} rows)\n", table, expressions.len());
            }
            Self::KeyLookup { table, alias: _, keys } => {
                s += &format!("KeyLookup: {}", table);
                if !keys.is_empty() && keys.len() < 10 {
                    s += &format!(
                        " ({})",
                        keys.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")
                    );
                } else {
                    s += &format!(" ({} keys)", keys.len());
                }
                s += "\n";
            }
            Self::Limit { source, limit } => {
                s += &format!("Limit: {}\n", limit);
                s += &source.format(indent, false, true);
            }
            Self::NestedLoopJoin { left, left_size: _, right, predicate, pad, flip } => {
                s += "NestedLoopJoin:";
                if !pad {
                    s += " inner";
                } else if !flip {
                    s += " left outer";
                } else if *flip {
                    s += " right outer";
                };
                if let Some(expr) = predicate {
                    s += &format!(" on {}", expr);
                }
                s += "\n";
                s += &left.format(indent.clone(), false, false);
                s += &right.format(indent, false, true);
            }
            Self::Nothing {} => {
                s += "Nothing\n";
            }
            Self::Offset { source, offset } => {
                s += &format!("Offset: {}\n", offset);
                s += &source.format(indent, false, true);
            }
            Self::Order { source, orders } => {
                s += &format!(
                    "Order: {}\n",
                    orders
                        .iter()
                        .map(|(expr, dir)| format!("{} {}", expr, dir))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                s += &source.format(indent, false, true);
            }
            Self::Projection { source, expressions } => {
                s += &format!(
                    "Projection: {}\n",
                    expressions
                        .iter()
                        .map(|(expr, _)| expr.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                s += &source.format(indent, false, true);
            }
            Self::Scan { table, alias: _, filter } => {
                s += &format!("Scan: {}", table);
                if let Some(expr) = filter {
                    s += &format!(" ({})", expr);
                }
                s += "\n";
            }
            Self::Update { source, table, expressions } => {
                s += &format!(
                    "Update: {} ({})\n",
                    table,
                    expressions
                        .iter()
                        .map(|(l, e)| format!("{}={}", l, e))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                s += &source.format(indent, false, true);
            }
        };
        if root {
            s = s.trim_end().to_string()
        }
        s
    }
}

impl Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format("".into(), true, true))
    }
}

/// An aggregate operation
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Aggregate {
    Average,
    Count,
    Max,
    Min,
    Sum,
}

impl Display for Aggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Average => "average",
                Self::Count => "count",
                Self::Max => "maximum",
                Self::Min => "minimum",
                Self::Sum => "sum",
            }
        )
    }
}

pub type Aggregates = Vec<Aggregate>;

/// A sort order direction
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Direction {
    Ascending,
    Descending,
}

impl Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Ascending => "asc",
                Self::Descending => "desc",
            }
        )
    }
}
