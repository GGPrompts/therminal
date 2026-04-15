@echo off
rem tn.cmd — Windows CMD wrapper around the `therminal` CLI.
rem
rem Windows equivalent of scripts/tn (bash). See that file for the
rem CLI-vs-MCP usage policy and full subcommand reference.
rem
rem Installation: copy this file (or the tn.exe alias) to a directory
rem on %%PATH%%. The .exe copy is preferred — it works in both CMD and
rem PowerShell without execution-policy concerns.

therminal %*
