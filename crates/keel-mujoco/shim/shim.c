// The few calls keel-mujoco makes of MuJoCo, in C: whatever reads or writes
// MuJoCo's structs lives here, compiled against its headers, so the Rust
// side never mirrors a struct layout. Also a headless OpenGL context (EGL,
// loaded at run time) for MuJoCo's classic renderer.
//
// Errors come back as a nonzero return and a message in `err`.

#include <dlfcn.h>
#include <math.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <mujoco/mujoco.h>

typedef struct {
  mjModel* m;
  mjData* d;
  // Positions, sensors and the scene are a step behind after mj_step: a
  // forward pass brings them to the present, when someone looks.
  int stale;
  // Rendering, once asked for.
  int rendering;
  mjvScene scn;
  mjvCamera cam;
  mjvOption opt;
  mjrContext con;
} kmj;

// MuJoCo's errors end the process unless handled. While a call that can
// report them is under way (`armed`), they jump back to it; otherwise, as
// in the middle of a step, they end the process with the message.
static jmp_buf error_jump;
static int armed = 0;
static char error_text[512];
static void on_error(const char* msg) {
  if (!armed) {
    fprintf(stderr, "MuJoCo: %s\n", msg);
    exit(1);
  }
  snprintf(error_text, sizeof error_text, "%s", msg);
  armed = 0;
  longjmp(error_jump, 1);
}
static void on_warning(const char* msg) { fprintf(stderr, "MuJoCo: %s\n", msg); }

static void say(char* err, int len, const char* what, const char* detail) {
  snprintf(err, len, "%s%s%s", what, detail && *detail ? ": " : "", detail ? detail : "");
}

static void handle_errors(void) {
  mju_user_error = on_error;
  mju_user_warning = on_warning;
}

// ---- building a model ------------------------------------------------------

mjSpec* kmj_parse(const char* path, char* err, int len) {
  handle_errors();
  char text[1000] = "";
  mjSpec* spec = mj_parseXML(path, NULL, text, sizeof text);
  if (!spec) say(err, len, "can't read the scene", text);
  return spec;
}

void kmj_free_spec(mjSpec* spec) { mj_deleteSpec(spec); }

// A site on `body`, to mount sensors on.
int kmj_mount_site(mjSpec* spec, const char* body, const char* name, const double pos[3],
                   const double quat[4], char* err, int len) {
  mjsBody* b = mjs_findBody(spec, body);
  if (!b) { say(err, len, "no body in the scene called", body); return 1; }
  mjsSite* site = mjs_addSite(b, NULL);
  mjs_setName(site->element, name);
  memcpy(site->pos, pos, 3 * sizeof(double));
  memcpy(site->quat, quat, 4 * sizeof(double));
  return 0;
}

// A sensor at a site: `kind` is one of accelerometer, gyro, force, torque,
// rangefinder.
int kmj_mount_sensor(mjSpec* spec, const char* kind, const char* site, const char* name, char* err,
                     int len) {
  static const struct { const char* name; mjtSensor type; } kinds[] = {
      {"accelerometer", mjSENS_ACCELEROMETER}, {"gyro", mjSENS_GYRO},
      {"force", mjSENS_FORCE},                 {"torque", mjSENS_TORQUE},
      {"rangefinder", mjSENS_RANGEFINDER},
  };
  for (unsigned i = 0; i < sizeof kinds / sizeof kinds[0]; i++) {
    if (strcmp(kind, kinds[i].name) == 0) {
      mjsSensor* sensor = mjs_addSensor(spec);
      mjs_setName(sensor->element, name);
      sensor->type = kinds[i].type;
      sensor->objtype = mjOBJ_SITE;
      mjs_setString(sensor->objname, site);
      return 0;
    }
  }
  say(err, len, "no such kind of sensor (accelerometer, gyro, force, torque, rangefinder)", kind);
  return 1;
}

// A camera on `body`, looking down its -z, y up, as MuJoCo's cameras do.
int kmj_mount_camera(mjSpec* spec, const char* body, const char* name, const double pos[3],
                     const double quat[4], double fovy, char* err, int len) {
  mjsBody* b = mjs_findBody(spec, body);
  if (!b) { say(err, len, "no body in the scene called", body); return 1; }
  mjsCamera* cam = mjs_addCamera(b, NULL);
  mjs_setName(cam->element, name);
  memcpy(cam->pos, pos, 3 * sizeof(double));
  memcpy(cam->quat, quat, 4 * sizeof(double));
  cam->fovy = fovy;
  return 0;
}

kmj* kmj_compile(mjSpec* spec, const char* keyframe, char* err, int len) {
  handle_errors();
  if (setjmp(error_jump)) { say(err, len, "MuJoCo", error_text); return NULL; }
  armed = 1;
  mjModel* m = mj_compile(spec, NULL);
  armed = 0;
  if (!m) { say(err, len, "can't compile the scene", mjs_getError(spec)); return NULL; }
  kmj* h = calloc(1, sizeof *h);
  h->m = m;
  h->d = mj_makeData(m);
  if (keyframe && *keyframe) {
    int key = mj_name2id(m, mjOBJ_KEY, keyframe);
    if (key < 0) {
      say(err, len, "no keyframe called", keyframe);
      mj_deleteData(h->d), mj_deleteModel(m), free(h);
      return NULL;
    }
    mj_resetDataKeyframe(m, h->d, key);
  }
  mj_forward(m, h->d);
  return h;
}

void kmj_free(kmj* h) {
  if (h->rendering) mjr_freeContext(&h->con), mjv_freeScene(&h->scn);
  mj_deleteData(h->d);
  mj_deleteModel(h->m);
  free(h);
}

// ---- time and joints -------------------------------------------------------

double kmj_timestep(const kmj* h) { return h->m->opt.timestep; }
double kmj_time(const kmj* h) { return h->d->time; }

void kmj_step(kmj* h) {
  mj_step(h->m, h->d);
  h->stale = 1;
}

static void present(kmj* h) {
  if (h->stale) mj_forward(h->m, h->d), h->stale = 0;
}

// The joints are the actuators', one each.
int kmj_joints(const kmj* h) { return h->m->nu; }

const char* kmj_joint_name(const kmj* h, int i) {
  const char* name = mj_id2name(h->m, mjOBJ_ACTUATOR, i);
  return name ? name : "";
}

// Actuator i's joint: position and velocity. 1 if it doesn't drive a joint.
int kmj_joint(const kmj* h, int i, double* pos, double* vel) {
  const mjModel* m = h->m;
  if (m->actuator_trntype[i] != mjTRN_JOINT) return 1;
  int j = m->actuator_trnid[2 * i];
  *pos = h->d->qpos[m->jnt_qposadr[j]];
  *vel = h->d->qvel[m->jnt_dofadr[j]];
  return 0;
}

// Makes actuator i a plain motor (its control is the torque) and says what
// servo it was: a position actuator's gains, and the most force it had. The
// drives are keel's to model now.
void kmj_take_over(kmj* h, int i, double* kp, double* kv, double* limit) {
  mjModel* m = h->m;
  *kp = m->actuator_gainprm[i * mjNGAIN];
  *kv = -m->actuator_biasprm[i * mjNBIAS + 2];
  double lo = m->actuator_forcerange[2 * i], hi = m->actuator_forcerange[2 * i + 1];
  *limit = m->actuator_forcelimited[i] ? fmax(fabs(lo), fabs(hi)) : 0;
  m->actuator_dyntype[i] = mjDYN_NONE;
  m->actuator_gaintype[i] = mjGAIN_FIXED;
  m->actuator_biastype[i] = mjBIAS_NONE;
  memset(m->actuator_gainprm + i * mjNGAIN, 0, mjNGAIN * sizeof(mjtNum));
  memset(m->actuator_biasprm + i * mjNBIAS, 0, mjNBIAS * sizeof(mjtNum));
  m->actuator_gainprm[i * mjNGAIN] = 1;
  m->actuator_ctrllimited[i] = 0;
}

void kmj_set_torque(kmj* h, int i, double torque) { h->d->ctrl[i] = torque; }

// ---- sensors ---------------------------------------------------------------

// A sensor's values into `out` (up to `max`); how many it has, or -1 if
// there's no such sensor.
int kmj_sensor(kmj* h, const char* name, double* out, int max) {
  int id = mj_name2id(h->m, mjOBJ_SENSOR, name);
  if (id < 0) return -1;
  present(h);
  int dim = h->m->sensor_dim[id];
  for (int k = 0; k < dim && k < max; k++) out[k] = h->d->sensordata[h->m->sensor_adr[id] + k];
  return dim;
}

// ---- rendering -------------------------------------------------------------

// EGL, loaded at run time: no headers, no link-time dependency.
typedef void* EGLDisplay;
typedef void* EGLConfig;
typedef void* EGLContext;
typedef void* EGLDeviceEXT;
#define EGL_PLATFORM_DEVICE_EXT 0x313F
#define EGL_NONE 0x3038
#define EGL_OPENGL_API 0x30A2

static int egl_context(char* err, int len) {
  static int done = 0;
  if (done) return 0;
  void* lib = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
  if (!lib) { say(err, len, "can't load libEGL.so.1", dlerror()); return 1; }
  void* (*getProc)(const char*) = (void* (*)(const char*))dlsym(lib, "eglGetProcAddress");
  EGLDisplay (*getDisplay)(void*) = (EGLDisplay(*)(void*))dlsym(lib, "eglGetDisplay");
  int (*initialize)(EGLDisplay, int*, int*) = (int (*)(EGLDisplay, int*, int*))dlsym(lib, "eglInitialize");
  int (*chooseConfig)(EGLDisplay, const int*, EGLConfig*, int, int*) =
      (int (*)(EGLDisplay, const int*, EGLConfig*, int, int*))dlsym(lib, "eglChooseConfig");
  int (*bindApi)(unsigned) = (int (*)(unsigned))dlsym(lib, "eglBindAPI");
  EGLContext (*createContext)(EGLDisplay, EGLConfig, EGLContext, const int*) =
      (EGLContext(*)(EGLDisplay, EGLConfig, EGLContext, const int*))dlsym(lib, "eglCreateContext");
  int (*makeCurrent)(EGLDisplay, void*, void*, EGLContext) =
      (int (*)(EGLDisplay, void*, void*, EGLContext))dlsym(lib, "eglMakeCurrent");
  if (!getProc || !getDisplay || !initialize || !chooseConfig || !bindApi || !createContext ||
      !makeCurrent) {
    say(err, len, "libEGL.so.1 lacks a function", NULL);
    return 1;
  }
  // A GPU without a window (a device), or else the default display.
  int (*queryDevices)(int, EGLDeviceEXT*, int*) =
      (int (*)(int, EGLDeviceEXT*, int*))getProc("eglQueryDevicesEXT");
  EGLDisplay (*platformDisplay)(unsigned, void*, const int*) =
      (EGLDisplay(*)(unsigned, void*, const int*))getProc("eglGetPlatformDisplayEXT");
  EGLDisplay display = NULL;
  EGLDeviceEXT devices[8];
  int ndevices = 0;
  if (queryDevices && platformDisplay && queryDevices(8, devices, &ndevices)) {
    for (int i = 0; i < ndevices && !display; i++) {
      EGLDisplay candidate = platformDisplay(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL);
      int major, minor;
      if (candidate && initialize(candidate, &major, &minor)) display = candidate;
    }
  }
  if (!display) {
    int major, minor;
    display = getDisplay(NULL);
    if (!display || !initialize(display, &major, &minor)) {
      say(err, len, "no EGL display: no GPU driver for headless rendering?", NULL);
      return 1;
    }
  }
  const int attributes[] = {
      0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8,  // red, green, blue, alpha
      0x3025, 24, 0x3026, 8,                       // depth, stencil
      0x303F, 0x308E,                              // colour buffer: RGB
      0x3033, 0x0001,                              // surface: pbuffer
      0x3040, 0x0008,                              // renderable: OpenGL
      EGL_NONE};
  EGLConfig config;
  int nconfigs = 0;
  if (!chooseConfig(display, attributes, &config, 1, &nconfigs) || nconfigs < 1) {
    say(err, len, "no suitable EGL configuration", NULL);
    return 1;
  }
  if (!bindApi(EGL_OPENGL_API)) { say(err, len, "EGL won't do OpenGL", NULL); return 1; }
  EGLContext context = createContext(display, config, NULL, NULL);
  if (!context || !makeCurrent(display, NULL, NULL, context)) {
    say(err, len, "can't make an EGL context", NULL);
    return 1;
  }
  done = 1;
  return 0;
}

// Gets the renderer ready for pictures up to `width` x `height`.
int kmj_render_init(kmj* h, int width, int height, char* err, int len) {
  if (h->rendering) return 0;
  if (egl_context(err, len)) return 1;
  handle_errors();
  if (setjmp(error_jump)) { say(err, len, "MuJoCo", error_text); return 1; }
  armed = 1;
  if (h->m->vis.global.offwidth < width) h->m->vis.global.offwidth = width;
  if (h->m->vis.global.offheight < height) h->m->vis.global.offheight = height;
  mjv_defaultScene(&h->scn);
  mjv_makeScene(h->m, &h->scn, 4000);
  mjv_defaultCamera(&h->cam);
  mjv_defaultOption(&h->opt);
  mjr_defaultContext(&h->con);
  mjr_makeContext(h->m, &h->con, mjFONTSCALE_150);
  mjr_setBuffer(mjFB_OFFSCREEN, &h->con);
  armed = 0;
  h->rendering = 1;
  return 0;
}

int kmj_camera(const kmj* h, const char* name) { return mj_name2id(h->m, mjOBJ_CAMERA, name); }

// What camera `cam` sees, rows from the top: colour into `rgb`, distance
// in metres into `depth` (either may be NULL).
void kmj_render(kmj* h, int cam, int width, int height, unsigned char* rgb, float* depth) {
  present(h);
  h->cam.type = mjCAMERA_FIXED;
  h->cam.fixedcamid = cam;
  mjv_updateScene(h->m, h->d, &h->opt, NULL, &h->cam, mjCAT_ALL, &h->scn);
  mjrRect viewport = {0, 0, width, height};
  mjr_render(viewport, &h->scn, &h->con);
  mjr_readPixels(rgb, depth, viewport, &h->con);
  // OpenGL's rows start at the bottom.
  if (rgb) {
    int row = 3 * width;
    unsigned char* tmp = malloc(row);
    for (int y = 0; y < height / 2; y++) {
      unsigned char *a = rgb + y * row, *b = rgb + (height - 1 - y) * row;
      memcpy(tmp, a, row), memcpy(a, b, row), memcpy(b, tmp, row);
    }
    free(tmp);
  }
  if (depth) {
    double extent = h->m->stat.extent;
    double near = h->m->vis.map.znear * extent, far = h->m->vis.map.zfar * extent;
    for (int y = 0; y < height / 2; y++) {
      for (int x = 0; x < width; x++) {
        float* a = depth + y * width + x;
        float* b = depth + (height - 1 - y) * width + x;
        float t = *a;
        *a = *b, *b = t;
      }
    }
    for (int k = 0; k < width * height; k++) {
      // The depth buffer's [0, 1] back to metres; the far plane is nothing.
      double z = depth[k];
      depth[k] = z >= 1.0 ? 0.0f : (float)(near / (1.0 - z * (1.0 - near / far)));
    }
  }
}
