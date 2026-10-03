/* A small Motif client for legacy-apps.sh: an XmMainWindow with a
 * menubar and a File pulldown, a scrolled XmList of 40 items, an XmText
 * and an XmPushButton, all in the "fixed" font at a fixed size.
 *
 *   ./legacy-motif X Y        # top-left corner of the shell
 *
 * Once mapped it writes its shell's window id to MOTIF-WINDOW; when the
 * file SCROLL appears it scrolls the list to item 25 and writes
 * MOTIF-SCROLLED; when the File pulldown pops up it writes the menu
 * shell's id to MOTIF-MENU, and MOTIF-MENU-DOWN when it pops down.
 *
 *   cc -O1 -o legacy-motif legacy-motif.c -lXm -lXt -lX11
 */
#include <Xm/CascadeB.h>
#include <Xm/Form.h>
#include <Xm/List.h>
#include <Xm/MainW.h>
#include <Xm/PushB.h>
#include <Xm/RowColumn.h>
#include <Xm/Text.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static Widget top, list;

static void note(const char *path, unsigned long id)
{
    FILE *f = fopen(path, "w");
    if (f) {
        fprintf(f, "0x%lx\n", id);
        fclose(f);
    }
}

static void poll_scroll(XtPointer data, XtIntervalId *id)
{
    (void)id;
    XtAppContext app = (XtAppContext)data;
    if (access("SCROLL", F_OK) == 0) {
        XmListSetPos(list, 25);
        XmUpdateDisplay(top);
        XSync(XtDisplay(top), False);
        note("MOTIF-SCROLLED", 1);
        return;
    }
    XtAppAddTimeOut(app, 100, poll_scroll, data);
}

static void mapped(XtPointer data, XtIntervalId *id)
{
    (void)id;
    XSync(XtDisplay(top), False);
    note("MOTIF-WINDOW", XtWindow(top));
    XtAppAddTimeOut((XtAppContext)data, 100, poll_scroll, data);
}

static void menu_up(Widget w, XtPointer client, XtPointer call)
{
    (void)client;
    (void)call;
    XSync(XtDisplay(w), False);
    note("MOTIF-MENU", XtWindow(XtParent(w)));
}

static void menu_down(Widget w, XtPointer client, XtPointer call)
{
    (void)client;
    (void)call;
    XSync(XtDisplay(w), False);
    note("MOTIF-MENU-DOWN", 1);
}

int main(int argc, char **argv)
{
    static String fallback[] = {"*renderTable: fixed", "*fontList: fixed",
                                "*blinkRate: 0", "*enableThinThickness: False", NULL};
    char geometry[64];
    snprintf(geometry, sizeof geometry, "300x280+%s+%s", argc > 1 ? argv[1] : "0",
             argc > 2 ? argv[2] : "0");
    XtAppContext app;
    top = XtVaAppInitialize(&app, "LegacyMotif", NULL, 0, &argc, argv, fallback,
                            XmNgeometry, geometry, NULL);
    Widget main_w = XmCreateMainWindow(top, "main", NULL, 0);
    Widget bar = XmCreateMenuBar(main_w, "bar", NULL, 0);
    Widget pulldown = XmCreatePulldownMenu(bar, "pulldown", NULL, 0);
    XtAddCallback(pulldown, XmNmapCallback, menu_up, NULL);
    XtAddCallback(pulldown, XmNunmapCallback, menu_down, NULL);
    const char *items[] = {"Open", "Save", "Quit"};
    for (int i = 0; i < 3; i++)
        XtManageChild(XmCreatePushButton(pulldown, (char *)items[i], NULL, 0));
    Widget cascade = XtVaCreateManagedWidget("File", xmCascadeButtonWidgetClass, bar,
                                             XmNsubMenuId, pulldown, NULL);
    (void)cascade;
    XtManageChild(bar);
    Widget form = XmCreateForm(main_w, "form", NULL, 0);
    XmString strings[40];
    char name[32];
    for (int i = 0; i < 40; i++) {
        snprintf(name, sizeof name, "list item %02d", i + 1);
        strings[i] = XmStringCreateLocalized(name);
    }
    Arg args[8];
    int n = 0;
    XtSetArg(args[n], XmNitems, strings), n++;
    XtSetArg(args[n], XmNitemCount, 40), n++;
    XtSetArg(args[n], XmNvisibleItemCount, 8), n++;
    XtSetArg(args[n], XmNtopAttachment, XmATTACH_FORM), n++;
    XtSetArg(args[n], XmNleftAttachment, XmATTACH_FORM), n++;
    XtSetArg(args[n], XmNrightAttachment, XmATTACH_FORM), n++;
    list = XmCreateScrolledList(form, "list", args, n);
    XtManageChild(list);
    Widget text = XtVaCreateManagedWidget(
        "text", xmTextWidgetClass, form, XmNvalue, "Motif text widget", XmNcolumns, 30,
        XmNcursorPositionVisible, False, XmNtopAttachment, XmATTACH_WIDGET, XmNtopWidget,
        XtParent(list), XmNleftAttachment, XmATTACH_FORM, XmNrightAttachment, XmATTACH_FORM,
        NULL);
    XtVaCreateManagedWidget("Push", xmPushButtonWidgetClass, form, XmNtopAttachment,
                            XmATTACH_WIDGET, XmNtopWidget, text, XmNleftAttachment,
                            XmATTACH_FORM, NULL);
    XtManageChild(form);
    XtManageChild(main_w);
    XtRealizeWidget(top);
    XtAppAddTimeOut(app, 1500, mapped, (XtPointer)app);
    XtAppMainLoop(app);
    return 0;
}
