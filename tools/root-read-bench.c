/* x11vnc's load on root reads: one client ShmPutImages a window at 60 Hz
 * ("video"), another polls the root in 32-row ShmGetImage strips, back to
 * back, for SECS seconds. Prints reads/s, per-read latency, the frames the
 * player managed and the server's CPU (utime+stime of PID from /proc).
 *
 *   ./root-read-bench idle|window|full SECS [SERVER_PID]
 *
 *   cc -O2 -o root-read-bench root-read-bench.c -lxcb -lxcb-shm
 */
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <xcb/shm.h>
#include <xcb/xcb.h>

#define STRIP 32

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static long server_ticks(int pid)
{
    char path[64], buf[1024];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    if (!f)
        return -1;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    long ut = 0, st = 0;
    if (!p || sscanf(p + 2, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %ld %ld", &ut, &st) != 2)
        return -1;
    return ut + st;
}

static xcb_shm_seg_t attach(xcb_connection_t *c, size_t size, uint8_t **mem)
{
    int id = shmget(IPC_PRIVATE, size, IPC_CREAT | 0600);
    if (id < 0) {
        perror("shmget");
        exit(1);
    }
    *mem = shmat(id, NULL, 0);
    xcb_shm_seg_t seg = xcb_generate_id(c);
    xcb_void_cookie_t ck = xcb_shm_attach_checked(c, seg, (uint32_t)id, 0);
    shmctl(id, IPC_RMID, NULL);
    if (xcb_request_check(c, ck)) {
        fprintf(stderr, "shm attach failed\n");
        exit(1);
    }
    return seg;
}

static void play(int full, int secs)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    uint16_t w = full ? s->width_in_pixels : 640, h = full ? s->height_in_pixels : 360;
    int16_t x = full ? 0 : 100, y = full ? 0 : 100;
    xcb_window_t win = xcb_generate_id(c);
    uint32_t vals[] = {0, 1};
    xcb_create_window(c, s->root_depth, win, s->root, x, y, w, h, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, vals);
    xcb_map_window(c, win);
    xcb_gcontext_t gc = xcb_generate_id(c);
    xcb_create_gc(c, gc, win, 0, NULL);
    uint8_t *mem;
    size_t size = (size_t)w * h * 4;
    xcb_shm_seg_t seg = attach(c, size, &mem);
    double next = now(), end = next + secs;
    unsigned frame = 0;
    while (now() < end) {
        uint32_t *px = (uint32_t *)mem;
        uint32_t base = frame * 0x010305u;
        for (size_t i = 0; i < (size_t)w * h; i++)
            px[i] = base + (uint32_t)i;
        xcb_shm_put_image(c, win, gc, w, h, 0, 0, w, h, 0, 0, s->root_depth,
                          XCB_IMAGE_FORMAT_Z_PIXMAP, 0, seg, 0);
        free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
        frame++;
        next += 1.0 / 60;
        double d = next - now();
        if (d > 0) {
            struct timespec ts = {0, (long)(d * 1e9)};
            nanosleep(&ts, NULL);
        } else {
            next = now();
        }
    }
    printf("player frames=%u fps=%.1f\n", frame, frame / (double)secs);
    fflush(stdout);
    xcb_disconnect(c);
}

static int cmp(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s idle|window|full SECS [SERVER_PID]\n", argv[0]);
        return 2;
    }
    const char *mode = argv[1];
    int secs = atoi(argv[2]);
    int pid = argc > 3 ? atoi(argv[3]) : 0;
    pid_t player = -1;
    if (strcmp(mode, "idle") != 0) {
        player = fork();
        if (player == 0) {
            play(strcmp(mode, "full") == 0, secs + 1);
            _exit(0);
        }
        sleep(1);
    }
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    uint16_t w = s->width_in_pixels, h = s->height_in_pixels;
    uint8_t *mem;
    xcb_shm_seg_t seg = attach(c, (size_t)w * STRIP * 4, &mem);
    size_t cap = 1 << 22, n = 0;
    double *lat = malloc(cap * sizeof *lat);
    long t0 = pid ? server_ticks(pid) : -1;
    double start = now(), end = start + secs;
    while (now() < end && n < cap) {
        for (uint16_t y = 0; y < h && n < cap; y += STRIP) {
            uint16_t rows = (uint16_t)(h - y < STRIP ? h - y : STRIP);
            double a = now();
            xcb_shm_get_image_reply_t *r = xcb_shm_get_image_reply(
                c, xcb_shm_get_image(c, s->root, 0, (int16_t)y, w, rows, ~0u,
                                     XCB_IMAGE_FORMAT_Z_PIXMAP, seg, 0), NULL);
            if (!r) {
                fprintf(stderr, "ShmGetImage failed at y=%u\n", y);
                return 1;
            }
            free(r);
            lat[n++] = now() - a;
        }
    }
    double el = now() - start;
    long t1 = pid ? server_ticks(pid) : -1;
    qsort(lat, n, sizeof *lat, cmp);
    double sum = 0;
    for (size_t i = 0; i < n; i++)
        sum += lat[i];
    printf("mode=%s root=%ux%u reads=%zu reads/s=%.0f lat_mean_us=%.0f lat_p50_us=%.0f "
           "lat_p99_us=%.0f",
           mode, w, h, n, n / el, sum / n * 1e6, lat[n / 2] * 1e6, lat[n * 99 / 100] * 1e6);
    if (t0 >= 0 && t1 >= 0)
        printf(" server_cpu=%.0f%%", (double)(t1 - t0) / sysconf(_SC_CLK_TCK) / el * 100);
    printf("\n");
    fflush(stdout);
    if (player > 0)
        waitpid(player, NULL, 0);
    xcb_disconnect(c);
    return 0;
}
