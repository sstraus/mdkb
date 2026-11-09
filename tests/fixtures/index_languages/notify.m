#import <Cocoa/Cocoa.h>
#include <CoreGraphics/CoreGraphics.h>

static NSArray* createBorderOverlays(NSColor *color);

// Quartz global coordinates start at the top left of the main display, y down;
// Cocoa starts at its bottom left, y up.
NSRect quartzToCocoaRect(CGRect rect, CGFloat mainDisplayHeight) {
    return NSMakeRect(rect.origin.x, mainDisplayHeight - (rect.origin.y + rect.size.height),
                      rect.size.width, rect.size.height);
}

static CGFloat mainDisplayHeight(void) {
    return CGDisplayBounds(CGMainDisplayID()).size.height;
}
static NSStatusBarButton* getStatusItemButton(void);

static NSTimer *controlIconTimer = nil;
static NSStatusBarButton *controlIconButton = nil;
static BOOL controlIconLit = NO;

static void stopControlIcon(void) {
    [controlIconTimer invalidate];
    controlIconTimer = nil;
    [controlIconButton.layer setBackgroundColor:nil];
    controlIconButton = nil;
    controlIconLit = NO;
}

static void startControlIcon(void) {
    if (controlIconTimer) return;
    controlIconButton = getStatusItemButton();
    if (!controlIconButton) return;
    [controlIconButton setWantsLayer:YES];
    controlIconButton.layer.cornerRadius = 7;
    controlIconLit = YES;
    controlIconButton.layer.backgroundColor = [[NSColor systemYellowColor] CGColor];
    controlIconTimer = [NSTimer scheduledTimerWithTimeInterval:0.4 repeats:YES block:^(NSTimer *timer) {
        controlIconLit = !controlIconLit;
        controlIconButton.layer.backgroundColor = controlIconLit ? [[NSColor systemYellowColor] CGColor] : nil;
    }];
}

static NSWindow *virtualCursor = nil;
static uint64_t virtualCursorGeneration = 0;

@interface VirtualCursorView : NSView
@end

@implementation VirtualCursorView
- (void)drawRect:(NSRect)dirtyRect {
    NSPoint tip = NSMakePoint(32, 32);
    NSBezierPath *halo = [NSBezierPath bezierPathWithOvalInRect:NSMakeRect(13, 13, 38, 38)];
    [[NSColor colorWithCalibratedRed:0.15 green:0.40 blue:1.0 alpha:0.26] setFill];
    [halo fill];
    [[NSColor colorWithCalibratedRed:0.10 green:0.37 blue:1.0 alpha:0.9] setStroke];
    [halo setLineWidth:2.5];
    [halo stroke];
    NSCursor *cursor = [NSCursor arrowCursor];
    NSImage *image = [cursor image];
    NSPoint hotSpot = [cursor hotSpot];
    [image drawAtPoint:NSMakePoint(tip.x - hotSpot.x, tip.y - image.size.height + hotSpot.y)
             fromRect:NSZeroRect operation:NSCompositingOperationSourceOver fraction:1.0];
}
@end

