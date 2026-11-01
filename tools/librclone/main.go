// Command librclone: rclone's librclone C API (github.com/rclone/rclone/librclone) with a chosen set of backends.
package main

/*
#include <stdlib.h>

struct RcloneRPCResult {
	char*	Output;
	int	Status;
};
*/
import "C"

import (
	"unsafe"

	"github.com/rclone/rclone/librclone/librclone"

	_ "github.com/rclone/rclone/fs/operations"
	_ "github.com/rclone/rclone/fs/sync"
)

//export RcloneInitialize
func RcloneInitialize() { librclone.Initialize() }

//export RcloneFinalize
func RcloneFinalize() { librclone.Finalize() }

type RcloneRPCResult struct {
	Output *C.char
	Status C.int
}

//export RcloneRPC
func RcloneRPC(method *C.char, input *C.char) (result C.struct_RcloneRPCResult) {
	output, status := librclone.RPC(C.GoString(method), C.GoString(input))
	result.Output = C.CString(output)
	result.Status = C.int(status)
	return result
}

//export RcloneFreeString
func RcloneFreeString(str *C.char) { C.free(unsafe.Pointer(str)) }

func main() {}
