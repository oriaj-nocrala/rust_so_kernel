// Minimal pthread_create() smoke test.
//
// Exercises the real sys_clone() path end to end: main() spawns a worker
// thread that increments a shared counter under a mutex, main joins it,
// then checks the result. This is the first userspace program to actually
// drive mlibc's generic thread.cpp (sys_prepare_stack + sys_clone +
// __mlibc_start_thread) against this kernel's clone(56) syscall.

#include <stdio.h>
#include <pthread.h>

static int counter = 0;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void *worker(void *arg) {
    int id = (int)(long)arg;
    printf("thread: worker %d started\n", id);

    for (int i = 0; i < 5; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }

    printf("thread: worker %d done\n", id);
    return (void *)(long)id;
}

int main(void) {
    printf("pthread_test: main starting\n");

    pthread_t threads[3];
    for (long i = 0; i < 3; i++) {
        int ret = pthread_create(&threads[i], NULL, worker, (void *)i);
        if (ret != 0) {
            printf("pthread_test: pthread_create failed: %d\n", ret);
            return 1;
        }
    }

    for (int i = 0; i < 3; i++) {
        void *retval = NULL;
        pthread_join(threads[i], &retval);
        printf("pthread_test: joined worker %ld (retval=%ld)\n",
               (long)i, (long)retval);
    }

    printf("pthread_test: counter = %d (expected 15)\n", counter);
    if (counter == 15) {
        printf("pthread_test: PASS\n");
    } else {
        printf("pthread_test: FAIL\n");
    }

    return 0;
}
