# Office batch fixture

`title-notes.pptx` was generated with python-pptx 1.0.2 for the Windows batch
tests. It contains two slides, a two-run Chinese title, a two-paragraph notes
body, an unchanged subtitle and an unchanged second-slide text box. It contains
no user data. Its source SHA-256 is
`6cac87a3e29de563714ec335c3f1b05ec3b63f982d3f56f53d64a690a90e4564`.

The parent workspace generator is
`pocs/poc-windows-office-batch/test_pptx_copy.py --rich --multiline`.
The server test includes this inert fixture at compile time and writes a copy
only into its temporary test directory. It never launches Office.
