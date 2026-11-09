//go:build darwin

package input

/*
#cgo CFLAGS: -x objective-c
#cgo LDFLAGS: -framework AppKit
#import <AppKit/AppKit.h>
#include <stdlib.h>

// Write text to the general pasteboard.
int setPasteboard(const char *text) {
    NSString *str = [NSString stringWithUTF8String:text];
    NSPasteboard *pb = [NSPasteboard generalPasteboard];
    [pb clearContents];
    BOOL ok = [pb setString:str forType:NSPasteboardTypeString];
    return ok ? 0 : 1;
}

// Read text from the general pasteboard. Caller must free the result.
const char* getPasteboard(void) {
    NSPasteboard *pb = [NSPasteboard generalPasteboard];
    NSString *str = [pb stringForType:NSPasteboardTypeString];
    if (str == nil) {
        return strdup("");
    }
    return strdup([str UTF8String]);
}
*/
import "C"

import (
	"fmt"
	"unsafe"
)

// SetClipboard writes text to the macOS general pasteboard.
func SetClipboard(text string) error {
	cstr := C.CString(text)
	defer C.free(unsafe.Pointer(cstr))
	if C.setPasteboard(cstr) != 0 {
		return fmt.Errorf("failed to write to pasteboard")
	}
	return nil
}

// GetClipboard reads text from the macOS general pasteboard.
func GetClipboard() (string, error) {
	cstr := C.getPasteboard()
	defer C.free(unsafe.Pointer(cstr))
	return C.GoString(cstr), nil
}

// PasteText writes text to the clipboard and simulates Cmd+V to paste it.
func PasteText(text string) error {
	return PasteTextWithGuard(text, nil)
}

// PasteTextWithGuard checks physical activity after updating the clipboard,
// immediately before it posts Cmd+V.
func PasteTextWithGuard(text string, guard func() bool) error {
	if err := SetClipboard(text); err != nil {
		return err
	}
	if guard != nil && !guard() {
		return ErrUserActive
	}
	if !KeyTap("v", []string{"cmd"}) {
		return fmt.Errorf("failed to simulate Cmd+V key tap")
	}
	return nil
}
