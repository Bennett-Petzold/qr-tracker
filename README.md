# QR-Tracker
Use QR codes to track attendance.

Uses the default camera to track students, mentors, and guests in qr-tracker.db.
QR codes are simply names or Guest\*.
This database has all information needed to reconstruct attendance,
e.g. to demonstrate there were always at least two adults present whenever a
student is in the build space.
It can be trivially copied off of the host machine.
The student and mentor lists must be manually initialized and updated.

## Adding Students and Mentors
Add the names as they appear in the QR codes to the mentors and students tables.
See `src/sqlite.rs::BackingDatabase::new` for table format.

## Transferring to a New Machine
Delete the `resolution` table.
Any resolution in that table not valid on a machine will cause crashes.
