#import <AppKit/AppKit.h>

static void print_usage(const char *program) {
    fprintf(stderr, "usage: %s <input-image> <size> <output.png> [inset]\n", program);
    fprintf(stderr, "       %s --check-transparent-corners <input.png>\n", program);
}

static BOOL corners_are_transparent(NSBitmapImageRep *bitmap, const char *path) {
    NSInteger width = bitmap.pixelsWide;
    NSInteger height = bitmap.pixelsHigh;
    if (width <= 0 || height <= 0) {
        fprintf(stderr, "error: invalid bitmap dimensions for %s\n", path);
        return NO;
    }

    NSPoint corners[] = {
        NSMakePoint(0, 0),
        NSMakePoint(width - 1, 0),
        NSMakePoint(0, height - 1),
        NSMakePoint(width - 1, height - 1),
    };
    for (NSUInteger index = 0; index < sizeof(corners) / sizeof(corners[0]); index++) {
        NSColor *color = [[bitmap colorAtX:corners[index].x y:corners[index].y]
            colorUsingColorSpace:NSColorSpace.deviceRGBColorSpace];
        if (color == nil || color.alphaComponent > 0.001) {
            fprintf(stderr, "error: corner %lu of %s is not transparent\n", (unsigned long)index, path);
            return NO;
        }
    }

    return YES;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc == 3 && strcmp(argv[1], "--check-transparent-corners") == 0) {
            NSData *data = [NSData dataWithContentsOfFile:[NSString stringWithUTF8String:argv[2]]];
            NSBitmapImageRep *bitmap = data == nil ? nil : [[NSBitmapImageRep alloc] initWithData:data];
            if (bitmap == nil) {
                fprintf(stderr, "error: could not load bitmap %s\n", argv[2]);
                return 1;
            }
            return corners_are_transparent(bitmap, argv[2]) ? 0 : 1;
        }

        if (argc != 4 && argc != 5) {
            print_usage(argv[0]);
            return 2;
        }

        NSString *inputPath = [NSString stringWithUTF8String:argv[1]];
        NSInteger size = [[NSString stringWithUTF8String:argv[2]] integerValue];
        NSString *outputPath = [NSString stringWithUTF8String:argv[3]];
        CGFloat inset = argc == 5 ? [[NSString stringWithUTF8String:argv[4]] doubleValue] : 0;

        if (size <= 0 || inset < 0 || inset * 2 >= size) {
            fprintf(stderr, "error: size and inset must define a positive drawing area\n");
            return 2;
        }

        NSImage *source = [[NSImage alloc] initWithContentsOfFile:inputPath];
        if (source == nil) {
            fprintf(stderr, "error: could not load %s\n", argv[1]);
            return 1;
        }

        NSBitmapImageRep *bitmap = [[NSBitmapImageRep alloc]
            initWithBitmapDataPlanes:NULL
                          pixelsWide:size
                          pixelsHigh:size
                       bitsPerSample:8
                     samplesPerPixel:4
                            hasAlpha:YES
                            isPlanar:NO
                      colorSpaceName:NSDeviceRGBColorSpace
                         bytesPerRow:0
                        bitsPerPixel:0];
        if (bitmap == nil) {
            fprintf(stderr, "error: could not allocate %ldx%ld bitmap\n", (long)size, (long)size);
            return 1;
        }

        memset(bitmap.bitmapData, 0, bitmap.bytesPerRow * bitmap.pixelsHigh);

        NSGraphicsContext *context = [NSGraphicsContext graphicsContextWithBitmapImageRep:bitmap];
        [NSGraphicsContext saveGraphicsState];
        [NSGraphicsContext setCurrentContext:context];
        context.imageInterpolation = NSImageInterpolationHigh;
        [source drawInRect:NSMakeRect(inset, inset, size - inset * 2, size - inset * 2)
                  fromRect:NSZeroRect
                 operation:NSCompositingOperationSourceOver
                  fraction:1.0
            respectFlipped:NO
                     hints:nil];
        [context flushGraphics];
        [NSGraphicsContext restoreGraphicsState];

        if (!corners_are_transparent(bitmap, argv[3])) return 1;

        NSData *png = [bitmap representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
        if (png == nil || ![png writeToFile:outputPath atomically:YES]) {
            fprintf(stderr, "error: could not write %s\n", argv[3]);
            return 1;
        }

        return 0;
    }
}
