# Transcript adapter sort: third-party notices

`src/pilotdeck-gateway/src/storage/transcript_adapter_sort.rs` adapts the stable TimSort run detection, binary insertion, gallop and merge decision ordering from V8 12.4.254's [`third_party/v8/builtins/array-sort.tq`](https://github.com/v8/v8/blob/12.4.254/third_party/v8/builtins/array-sort.tq). That file identifies its underlying algorithm as based on CPython and bears the following notice:

> Copyright (c) 2001, 2002, 2003, 2004, 2005, 2006, 2007, 2008, 2009, 2010, 2011, 2012, 2013, 2014, 2015, 2016, 2017, 2018 Python Software Foundation; All Rights Reserved.

Python material is governed by the Python Software Foundation License Version 2 and, where applicable, the additional historical licences reproduced in [cpython-3.7.0-LICENSE.txt](cpython-3.7.0-LICENSE.txt). This is the complete CPython 3.7.0 licence file, selected to retain the 2001–2018 notice; V8's file does not specify an exact CPython revision, so this is not a claim about its precise upstream revision.

V8 project material is governed by the 3-clause BSD terms and notices in [v8-12.4.254-LICENSE.txt](v8-12.4.254-LICENSE.txt). The V8 licence separately notes that externally maintained libraries may carry their own terms. The two licence texts are preserved separately; neither is presented as replacing the other.

**Modification summary:** The Torque implementation was adapted to Rust for sorting session records by descending numeric `lastModified`. The adapter sorts indexes and rearranges the owned records afterward, uses the ECMAScript comparator's NaN-as-zero behavior, and invokes this path only when a NaN is present. Generic JavaScript array access, holes, prototype behavior and arbitrary user callbacks are outside this implementation. The run detection, gallop and merge decisions are retained to reproduce Node 22 behavior for the non-transitive comparator.

These notices and licence texts must accompany source and binary distributions that include the adapted sort.
