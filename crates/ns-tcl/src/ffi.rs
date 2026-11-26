//! Minimal hand-written bindings to the Tcl 8.6 C API.
//!
//! Only the functions the engine needs are declared.  `Tcl_Obj` reference
//! counting uses the exported `Tcl_DbIncrRefCount`/`Tcl_DbDecrRefCount`
//! functions rather than the C macros, so no struct layout is assumed.

#![allow(non_camel_case_types, non_snake_case, missing_docs, dead_code)]

use std::os::raw::{c_char, c_int, c_void};

/// Opaque `Tcl_Interp`.
#[repr(C)]
pub struct Tcl_Interp {
    _private: [u8; 0],
}

/// Opaque `Tcl_Obj`.
#[repr(C)]
pub struct Tcl_Obj {
    _private: [u8; 0],
}

pub type Tcl_Command = *mut c_void;
/// Opaque `Tcl_Channel`.
pub type Tcl_Channel = *mut c_void;
pub type ClientData = *mut c_void;
pub type Tcl_ObjCmdProc =
    unsafe extern "C" fn(ClientData, *mut Tcl_Interp, c_int, *const *mut Tcl_Obj) -> c_int;
pub type Tcl_CmdDeleteProc = unsafe extern "C" fn(ClientData);

pub const TCL_OK: c_int = 0;
pub const TCL_ERROR: c_int = 1;
pub const TCL_GLOBAL_ONLY: c_int = 1;
pub const TCL_LEAVE_ERR_MSG: c_int = 0x200;
pub const TCL_EVAL_GLOBAL: c_int = 0x20000;
pub const TCL_READABLE: c_int = 1 << 1;
pub const TCL_WRITABLE: c_int = 1 << 2;

unsafe extern "C" {
    pub fn Tcl_FindExecutable(argv0: *const c_char);
    pub fn Tcl_CreateInterp() -> *mut Tcl_Interp;
    pub fn Tcl_DeleteInterp(interp: *mut Tcl_Interp);
    pub fn Tcl_Init(interp: *mut Tcl_Interp) -> c_int;
    pub fn Tcl_FinalizeThread();
    pub fn Tcl_EvalEx(
        interp: *mut Tcl_Interp,
        script: *const c_char,
        len: c_int,
        flags: c_int,
    ) -> c_int;
    pub fn Tcl_EvalObjv(
        interp: *mut Tcl_Interp,
        objc: c_int,
        objv: *const *mut Tcl_Obj,
        flags: c_int,
    ) -> c_int;
    pub fn Tcl_GetObjResult(interp: *mut Tcl_Interp) -> *mut Tcl_Obj;
    pub fn Tcl_SetObjResult(interp: *mut Tcl_Interp, obj: *mut Tcl_Obj);
    pub fn Tcl_ResetResult(interp: *mut Tcl_Interp);
    pub fn Tcl_NewStringObj(bytes: *const c_char, len: c_int) -> *mut Tcl_Obj;
    pub fn Tcl_NewListObj(objc: c_int, objv: *const *mut Tcl_Obj) -> *mut Tcl_Obj;
    pub fn Tcl_ListObjGetElements(
        interp: *mut Tcl_Interp,
        list: *mut Tcl_Obj,
        objc: *mut c_int,
        objv: *mut *mut *mut Tcl_Obj,
    ) -> c_int;
    pub fn Tcl_GetStringFromObj(obj: *mut Tcl_Obj, len: *mut c_int) -> *mut c_char;
    pub fn Tcl_DbIncrRefCount(obj: *mut Tcl_Obj, file: *const c_char, line: c_int);
    pub fn Tcl_DbDecrRefCount(obj: *mut Tcl_Obj, file: *const c_char, line: c_int);
    pub fn Tcl_CreateObjCommand(
        interp: *mut Tcl_Interp,
        name: *const c_char,
        proc_: Tcl_ObjCmdProc,
        cd: ClientData,
        delete: Option<Tcl_CmdDeleteProc>,
    ) -> Tcl_Command;
    pub fn Tcl_DeleteCommand(interp: *mut Tcl_Interp, name: *const c_char) -> c_int;
    pub fn Tcl_GetCommandInfo(
        interp: *mut Tcl_Interp,
        name: *const c_char,
        info: *mut c_void,
    ) -> c_int;
    pub fn Tcl_SetVar2Ex(
        interp: *mut Tcl_Interp,
        part1: *const c_char,
        part2: *const c_char,
        value: *mut Tcl_Obj,
        flags: c_int,
    ) -> *mut Tcl_Obj;
    pub fn Tcl_GetVar2Ex(
        interp: *mut Tcl_Interp,
        part1: *const c_char,
        part2: *const c_char,
        flags: c_int,
    ) -> *mut Tcl_Obj;
    pub fn Tcl_UnsetVar2(
        interp: *mut Tcl_Interp,
        part1: *const c_char,
        part2: *const c_char,
        flags: c_int,
    ) -> c_int;
    pub fn Tcl_SetObjErrorCode(interp: *mut Tcl_Interp, code: *mut Tcl_Obj);
    pub fn Tcl_GetVersion(
        major: *mut c_int,
        minor: *mut c_int,
        patch: *mut c_int,
        kind: *mut c_int,
    );
    pub fn Tcl_GetChannel(
        interp: *mut Tcl_Interp,
        name: *const c_char,
        mode: *mut c_int,
    ) -> Tcl_Channel;
    pub fn Tcl_GetChannelHandle(
        chan: Tcl_Channel,
        direction: c_int,
        handle: *mut ClientData,
    ) -> c_int;
    pub fn Tcl_GetChannelName(chan: Tcl_Channel) -> *const c_char;
    pub fn Tcl_InputBuffered(chan: Tcl_Channel) -> c_int;
    pub fn Tcl_MakeTcpClientChannel(sock: ClientData) -> Tcl_Channel;
    pub fn Tcl_RegisterChannel(interp: *mut Tcl_Interp, chan: Tcl_Channel);
    pub fn Tcl_UnregisterChannel(interp: *mut Tcl_Interp, chan: Tcl_Channel) -> c_int;
    pub fn Tcl_SetChannelOption(
        interp: *mut Tcl_Interp,
        chan: Tcl_Channel,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
}
