 # innodb_controldata

`innodb_controldata` is a Rust command-line project for examining control
information stored in InnoDB data files. It’s like the Postgres `pg_controldata` for InnoDB

## Run

Use Cargo to display the options supported by the current version:

```
cargo run -- --help
```

Follow the displayed usage to specify the InnoDB file to inspect. Work with a
copy of important database files and keep backups

## Sample run and Output
```
user@ubuntu-bionic:~/Rust/innodb_controldata$ sudo -u mysql -s
mysql@ubuntu-bionic:/home/user/Rust/innodb_controldata$ cargo run /var/lib/mysql
Redo log layout:                      #innodb_redo/#ib_redoN (8.0.30+)
Redo log format:                      6 (8.0.30+)
Redo log creator:                     MySQL 9.5.0
Redo log UUID:                        1316808119
Redo header flags:                    0x00000000
Redo files:                           1 (capacity 3274752 bytes)
Database state:                       shut down cleanly

Latest checkpoint LSN:                11817183351
Latest checkpoint location:           #innodb_redo/#ib_redo3608 @ 1871991
Latest checkpoint slot:               2 (in #innodb_redo/#ib_redo3608)

Redo log end LSN:                     11817183351
Redo log end location:                #innodb_redo/#ib_redo3608 @ 1871991
Checkpoint age (bytes):               0
Redo blocks scanned:                  1
ibdata1 flushed LSN:                  11817123096
mysql@ubuntu-bionic:/home/user/Rust/innodb_controldata$
```

### What it reports
**Latest checkpoint LSN:** it reads both checkpoint slots and keeps the newest one that passes its checksum. It also shows which file and byte offset the checkpoint is at.

**Redo log end LSN:** it walks the log blocks forward from the checkpoint until it reaches a block that fails its checksum or block-number check or one that isn’t full. That’s the same way recovery finds the end of the log.

**Checkpoint age (end minus checkpoint) and a database state line:** “shut down cleanly” when the end equals the checkpoint otherwise “in production or crashed”.

**ibdata1 flushed LSN** from page 0 of the system tablespace. The data files have been flushed upto this location

### It handles two redo layouts
**MySQL 8.0.30+**: the #innodb_redo/#ib_redoN files. It skips the _tmp spare files and uses each file’s start LSN to find positions.

**MySQL 5.7.9 to 8.0.29**: the circular ib_logfile0..N group. It uses the checkpoint’s stored offset to find positions including when the log wraps across files.