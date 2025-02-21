use super::super::schema::{Catalog, Table, Tables};
use super::super::types::{Expression, Row, Value};
use super::Transaction as _;
use crate::error::{Error, Result};
use crate::storage::kv;

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::clone::Clone;
use std::collections::HashSet;

/// A SQL engine based on an underlying MVCC key/value store
/// 一个基于MVCC的sql引擎
pub struct KV {
    /// The underlying key/value store
    pub(super) kv: kv::MVCC,
}

// FIXME Implement Clone manually due to https://github.com/rust-lang/rust/issues/26925
impl Clone for KV {
    fn clone(&self) -> Self {
        KV::new(self.kv.clone())
    }
}

impl KV {
    /// Creates a new key/value-based SQL engine
    pub fn new(kv: kv::MVCC) -> Self {
        Self { kv }
    }

    /// Fetches an unversioned metadata value
    pub fn get_metadata(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.kv.get_metadata(key)
    }

    /// Sets an unversioned metadata value
    pub fn set_metadata(&self, key: &[u8], value: Vec<u8>) -> Result<()> {
        self.kv.set_metadata(key, value)
    }
}

// KV还是一个sql引擎
impl super::Engine for KV {
    type Transaction = Transaction;

    fn begin(&self, mode: super::Mode) -> Result<Self::Transaction> {
        Ok(Self::Transaction::new(self.kv.begin_with_mode(mode)?))
    }

    fn resume(&self, id: u64) -> Result<Self::Transaction> {
        Ok(Self::Transaction::new(self.kv.resume(id)?))
    }
}

/// Serializes SQL metadata.
fn serialize<V: Serialize>(value: &V) -> Result<Vec<u8>> {
    Ok(bincode::serialize(value)?)
}

/// Deserializes SQL metadata.
fn deserialize<'a, V: Deserialize<'a>>(bytes: &'a [u8]) -> Result<V> {
    Ok(bincode::deserialize(bytes)?)
}

/// An SQL transaction based on an MVCC key/value transaction
/// 一个以mvcc事务为基础的sql事务
pub struct Transaction {
    txn: kv::mvcc::Transaction,
}

impl Transaction {
    /// Creates a new SQL transaction from an MVCC transaction
    fn new(txn: kv::mvcc::Transaction) -> Self {
        Self { txn }
    }

    /// Loads an index entry
    fn index_load(&self, table: &str, column: &str, value: &Value) -> Result<HashSet<Value>> {
        Ok(self
            .txn
            .get(&Key::Index(table.into(), column.into(), Some(value.into())).encode())?
            .map(|v| deserialize(&v))
            .transpose()?
            .unwrap_or_else(HashSet::new))
    }

    /// Saves an index entry.
    fn index_save(
        &mut self,
        table: &str,
        column: &str,
        value: &Value,
        index: HashSet<Value>,
    ) -> Result<()> {
        let key = Key::Index(table.into(), column.into(), Some(value.into())).encode();
        if index.is_empty() {
            self.txn.delete(&key)
        } else {
            self.txn.set(&key, serialize(&index)?)
        }
    }
}

/// 这个事务还是sql事务，即mod中定义的sql事务
impl super::Transaction for Transaction {
    fn id(&self) -> u64 {
        self.txn.id()
    }

    fn mode(&self) -> super::Mode {
        self.txn.mode()
    }

    fn commit(self) -> Result<()> {
        self.txn.commit()
    }

    fn rollback(self) -> Result<()> {
        self.txn.rollback()
    }

    fn create(&mut self, table: &str, row: Row) -> Result<()> {
        let table = self.must_read_table(table)?;
        table.validate_row(&row, self)?;
        let id = table.get_row_key(&row)?;
        if self.read(&table.name, &id)?.is_some() {
            return Err(Error::Value(format!(
                "Primary key {} already exists for table {}",
                id, table.name
            )));
        }
        self.txn.set(
            &Key::Row(Cow::Borrowed(&table.name), Some(Cow::Borrowed(&id))).encode(),
            serialize(&row)?,
        )?;

        // Update indexes
        // 写入完成后需要更新索引
        for (i, column) in table.columns.iter().enumerate().filter(|(_, c)| c.index) {
            // 索引因素：表名、列名、要构建索引的值
            let mut index = self.index_load(&table.name, &column.name, &row[i])?;
            // 将主键也加入索引
            index.insert(id.clone());
            self.index_save(&table.name, &column.name, &row[i], index)?;
        }
        Ok(())
    }

    fn delete(&mut self, table: &str, id: &Value) -> Result<()> {
        let table = self.must_read_table(table)?;
        for (t, cs) in self.table_references(&table.name, true)? {
            let t = self.must_read_table(&t)?;
            // 获取所有reference要删除的table的表
            let cs = cs
                .into_iter()
                .map(|c| Ok((t.get_column_index(&c)?, c)))
                .collect::<Result<Vec<_>>>()?;
            // 扫描跟它有关联的表
            let mut scan = self.scan(&t.name, None)?;
            while let Some(row) = scan.next().transpose()? {
                // 获取该列信息
                for (i, c) in &cs {
                    // 如果另一张表中有一段数据跟要删除的数据关联，则删除失败
                    if &row[*i] == id && (table.name != t.name || id != &table.get_row_key(&row)?) {
                        return Err(Error::Value(format!(
                            "Primary key {} is referenced by table {} column {}",
                            id, t.name, c
                        )));
                    }
                }
            }
        }

        // 删除索引
        let indexes: Vec<_> = table.columns.iter().enumerate().filter(|(_, c)| c.index).collect();
        if !indexes.is_empty() {
            if let Some(row) = self.read(&table.name, id)? {
                for (i, column) in indexes {
                    // 根据表-列-列值获取索引
                    let mut index = self.index_load(&table.name, &column.name, &row[i])?;
                    index.remove(id);
                    self.index_save(&table.name, &column.name, &row[i], index)?;
                }
            }
        }
        self.txn.delete(&Key::Row(table.name.into(), Some(id.into())).encode())
    }

    fn read(&self, table: &str, id: &Value) -> Result<Option<Row>> {
        self.txn
            .get(&Key::Row(table.into(), Some(id.into())).encode())?
            .map(|v| deserialize(&v))
            .transpose()
    }

    fn read_index(&self, table: &str, column: &str, value: &Value) -> Result<HashSet<Value>> {
        if !self.must_read_table(table)?.get_column(column)?.index {
            // 如果该列不是索引则抛出错误
            return Err(Error::Value(format!("No index on {}.{}", table, column)));
        }
        self.index_load(table, column, value)
    }

    fn scan(&self, table: &str, filter: Option<Expression>) -> Result<super::Scan> {
        let table = self.must_read_table(table)?;
        Ok(Box::new(
            self.txn
                .scan_prefix(&Key::Row((&table.name).into(), None).encode())?
                .map(|r| r.and_then(|(_, v)| deserialize(&v)))
                .filter_map(move |r| match r {
                    Ok(row) => match &filter {
                        Some(filter) => match filter.evaluate(Some(&row)) {
                            Ok(Value::Boolean(b)) if b => Some(Ok(row)),
                            Ok(Value::Boolean(_)) | Ok(Value::Null) => None,
                            Ok(v) => Some(Err(Error::Value(format!(
                                "Filter returned {}, expected boolean",
                                v
                            )))),
                            Err(err) => Some(Err(err)),
                        },
                        None => Some(Ok(row)),
                    },
                    err => Some(err),
                }),
        ))
    }

    fn scan_index(&self, table: &str, column: &str) -> Result<super::IndexScan> {
        let table = self.must_read_table(table)?;
        let column = table.get_column(column)?;
        if !column.index {
            return Err(Error::Value(format!("No index for {}.{}", table.name, column.name)));
        }
        Ok(Box::new(
            self.txn
                // 从store中查询Index
                .scan_prefix(
                    &Key::Index((&table.name).into(), (&column.name).into(), None).encode(),
                )?
                .map(|r| -> Result<(Value, HashSet<Value>)> {
                    // 索引的id和对应的值
                    let (k, v) = r?;
                    // 获取索引的id
                    let value = match Key::decode(&k)? {
                        Key::Index(_, _, Some(pk)) => pk.into_owned(),
                        _ => return Err(Error::Internal("Invalid index key".into())),
                    };
                    Ok((value, deserialize(&v)?))
                }),
        ))
    }

    fn update(&mut self, table: &str, id: &Value, row: Row) -> Result<()> {
        let table = self.must_read_table(table)?;
        // If the primary key changes we do a delete and create, otherwise we replace the row
        // 如果更新的数据主键和where中的id不一致，则删除id的字段然后创建row
        if id != &table.get_row_key(&row)? {
            self.delete(&table.name, id)?;
            self.create(&table.name, row)?;
            return Ok(());
        }

        // Update indexes, knowing that the primary key has not changed
        // 更新索引,已知主键未发生变化
        let indexes: Vec<_> = table.columns.iter().enumerate().filter(|(_, c)| c.index).collect();
        if !indexes.is_empty() {
            let old = self.read(&table.name, id)?.unwrap();
            for (i, column) in indexes {
                if old[i] == row[i] {
                    // 如果新要更新的数据中存在索引的列的值和原来的值相同，则跳过
                    continue;
                }
                let mut index = self.index_load(&table.name, &column.name, &old[i])?;
                index.remove(id);
                self.index_save(&table.name, &column.name, &old[i], index)?;

                let mut index = self.index_load(&table.name, &column.name, &row[i])?;
                index.insert(id.clone());
                self.index_save(&table.name, &column.name, &row[i], index)?;
            }
        }

        table.validate_row(&row, self)?;
        self.txn.set(&Key::Row(table.name.into(), Some(id.into())).encode(), serialize(&row)?)
    }
}

/// sql事务可以查看存储信息
impl Catalog for Transaction {
    fn create_table(&mut self, table: Table) -> Result<()> {
        if self.read_table(&table.name)?.is_some() {
            return Err(Error::Value(format!("Table {} already exists", table.name)));
        }
        table.validate(self)?;
        self.txn.set(&Key::Table(Some((&table.name).into())).encode(), serialize(&table)?)
    }

    fn delete_table(&mut self, table: &str) -> Result<()> {
        let table = self.must_read_table(table)?;
        if let Some((t, cs)) = self.table_references(&table.name, false)?.first() {
            // 只要有一张表关联了要删除的表，则删除失败
            return Err(Error::Value(format!(
                "Table {} is referenced by table {} column {}",
                table.name, t, cs[0]
            )));
        }
        // 遍历这张表的所有数据,因此filter为None
        let mut scan = self.scan(&table.name, None)?;
        // 分别删除
        while let Some(row) = scan.next().transpose()? {
            self.delete(&table.name, &table.get_row_key(&row)?)?
        }
        self.txn.delete(&Key::Table(Some(table.name.into())).encode())
    }

    fn read_table(&self, table: &str) -> Result<Option<Table>> {
        self.txn.get(&Key::Table(Some(table.into())).encode())?.map(|v| deserialize(&v)).transpose()
    }

    fn scan_tables(&self) -> Result<Tables> {
        Ok(Box::new(
            self.txn
                .scan_prefix(&Key::Table(None).encode())?
                .map(|r| r.and_then(|(_, v)| deserialize(&v)))
                .collect::<Result<Vec<_>>>()?
                .into_iter(),
        ))
    }
}

/// Encodes SQL keys, using an order-preserving encoding - see kv::encoding for details. Options can
/// be None to get a keyspace prefix. We use table and column names directly as identifiers, to
/// avoid additional indirection and associated overhead. It is not possible to change names, so
/// this is ok. Uses Cows since we want to borrow when encoding but return owned when decoding.
enum Key<'a> {
    /// A table schema key for the given table name
    /// 记录表格元数据的key
    Table(Option<Cow<'a, str>>),
    /// A key for an index entry
    /// 索引的key
    Index(Cow<'a, str>, Cow<'a, str>, Option<Cow<'a, Value>>),
    /// A key for a row identified by table name and row primary key
    /// 表格内容的key,由表名、主键值组成
    Row(Cow<'a, str>, Option<Cow<'a, Value>>),
}

impl<'a> Key<'a> {
    /// Encodes the key as a byte vector
    fn encode(self) -> Vec<u8> {
        use kv::encoding::*;
        match self {
            Self::Table(None) => vec![0x01],
            Self::Table(Some(name)) => [&[0x01][..], &encode_string(&name)].concat(),
            Self::Index(table, column, None) => {
                [&[0x02][..], &encode_string(&table), &encode_string(&column)].concat()
            }
            Self::Index(table, column, Some(value)) => [
                &[0x02][..],
                &encode_string(&table),
                &encode_string(&column),
                &encode_value(&value),
            ]
            .concat(),
            Self::Row(table, None) => [&[0x03][..], &encode_string(&table)].concat(),
            Self::Row(table, Some(pk)) => {
                [&[0x03][..], &encode_string(&table), &encode_value(&pk)].concat()
            }
        }
    }

    /// Decodes a key from a byte vector
    fn decode(mut bytes: &[u8]) -> Result<Self> {
        use kv::encoding::*;
        let bytes = &mut bytes;
        let key = match take_byte(bytes)? {
            0x01 => Self::Table(Some(take_string(bytes)?.into())),
            0x02 => Self::Index(
                take_string(bytes)?.into(),
                take_string(bytes)?.into(),
                Some(take_value(bytes)?.into()),
            ),
            0x03 => Self::Row(take_string(bytes)?.into(), Some(take_value(bytes)?.into())),
            b => return Err(Error::Internal(format!("Unknown SQL key prefix {:x?}", b))),
        };
        if !bytes.is_empty() {
            return Err(Error::Internal("Unexpected data remaining at end of key".into()));
        }
        Ok(key)
    }
}
