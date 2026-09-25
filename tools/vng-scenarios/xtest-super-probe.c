/* Issue #168: after `xdotool key super+5` under a WM that grabs Mod4+5,
 * does the server still believe Super is held? Prints the XKB modifier
 * state, the QueryKeymap held keycodes, and whether a keyboard grab is
 * still active (XGrabKeyboard answers AlreadyGrabbed). */
#include <stdio.h>
#include <X11/Xlib.h>
#include <X11/XKBlib.h>

int main(int argc, char **argv)
{
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    const char *tag = argc > 1 ? argv[1] : "";
    XkbStateRec st;
    XkbGetState(d, XkbUseCoreKbd, &st);
    char keys[32];
    XQueryKeymap(d, keys);
    printf("%s mods=0x%02x base=0x%02x latched=0x%02x locked=0x%02x compat=0x%02x grab_mods=0x%02x held=[",
           tag, st.mods, st.base_mods, st.latched_mods, st.locked_mods,
           st.compat_state, st.grab_mods);
    for (int k = 0; k < 256; k++)
        if (keys[k / 8] & (1 << (k % 8))) printf(" %d", k);
    int g = XGrabKeyboard(d, DefaultRootWindow(d), False, GrabModeAsync, GrabModeAsync, CurrentTime);
    printf(" ] grab_kbd=%s\n", g == GrabSuccess ? "Success" : g == AlreadyGrabbed ? "AlreadyGrabbed" : "other");
    XUngrabKeyboard(d, CurrentTime);
    XCloseDisplay(d);
    return 0;
}
