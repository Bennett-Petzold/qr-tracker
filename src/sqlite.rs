/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use chrono::{DateTime, Local};
use nokhwa::utils::Resolution;
use rusqlite::Connection;

#[derive(Debug)]
pub struct BackingDatabase {
    conn: Connection,
}

impl BackingDatabase {
    pub fn new(conn_file: Option<&str>) -> Self {
        let conn = if let Some(conn_file) = conn_file {
            Connection::open(conn_file)
        } else {
            Connection::open_in_memory()
        }
        .unwrap();

        // Initialize all the tables.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
BEGIN TRANSACTION;
CREATE TABLE IF NOT EXISTS attendance (
    name TEXT NOT NULL,
    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP NOT NULL,
    PRIMARY KEY (name, timestamp)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS current (
    name TEXT PRIMARY KEY NOT NULL,
    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP NOT NULL,
    present BOOLEAN NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS mentors (
    name TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS students (
    name TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS resolution (
    x INTEGER NOT NULL,
    y INTEGER NOT NULL,
    PRIMARY KEY (x, y)
) WITHOUT ROWID;

COMMIT;",
        )
        .unwrap();

        Self { conn }
    }

    pub fn add_scan(&mut self, name: &str, timestamp: DateTime<Local>) {
        let transaction = self.conn.transaction().unwrap();
        {
            let mut attendance_stmt = transaction
                .prepare_cached("INSERT INTO attendance (name, timestamp) VALUES (?1, ?2);")
                .unwrap();
            let mut current_stmt = transaction
                .prepare_cached(
                    "INSERT INTO current (name, timestamp, present) VALUES (?1, ?2, TRUE)
ON CONFLICT(name) DO UPDATE
SET timestamp = ?2, present = NOT present;",
                )
                .unwrap();

            attendance_stmt
                .execute((name, timestamp.timestamp()))
                .unwrap();
            current_stmt.execute((name, timestamp.timestamp())).unwrap();
        }
        transaction.commit().unwrap();
    }

    pub fn get_present(&self) -> Vec<(String, DateTime<Local>)> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name, timestamp FROM current WHERE present = TRUE;")
            .unwrap();

        let row_iter = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten();

        row_iter
            .map(|(name, timestamp)| {
                (
                    name,
                    DateTime::from_timestamp_secs(timestamp).unwrap().into(),
                )
            })
            .collect()
    }

    pub fn get_mentors(&self) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name FROM mentors;")
            .unwrap();

        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    }

    pub fn get_students(&self) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name FROM students;")
            .unwrap();

        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    }

    pub fn get_resolution(&self) -> Option<Resolution> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT x, y FROM resolution;")
            .unwrap();

        let res = stmt
            .query_map([], |row| Ok(Resolution::new(row.get(0)?, row.get(1)?)))
            .unwrap()
            .next()
            .map(|x| x.unwrap());
        res
    }

    pub fn set_resolution(&mut self, resolution: Resolution) {
        let transaction = self.conn.transaction().unwrap();
        {
            let mut delete_stmt = transaction
                .prepare_cached("DELETE FROM resolution;")
                .unwrap();
            let mut insert_stmt = transaction
                .prepare_cached("INSERT INTO resolution(x, y) VALUES (?1, ?2);")
                .unwrap();

            delete_stmt.execute(()).unwrap();
            insert_stmt
                .execute((resolution.x(), resolution.y()))
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    pub fn checkpoint(&self) {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .unwrap();
    }
}
