#include <assert.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>

/* Internal PulseAudio API, exported by the native dependency under test. */
extern void pa_reset_priority(void);

int main(void) {
    pthread_attr_t defaults;
    struct sched_param expected, current, depressed = {0};
    int expected_policy, current_policy;

    assert(pthread_attr_init(&defaults) == 0);
    assert(pthread_attr_getschedparam(&defaults, &expected) == 0);
    assert(pthread_attr_getschedpolicy(&defaults, &expected_policy) == 0);
    assert(pthread_attr_destroy(&defaults) == 0);
    assert(expected.sched_priority > 0);

    assert(pthread_setschedparam(pthread_self(), SCHED_OTHER, &depressed) == 0);
    pa_reset_priority();
    assert(pthread_getschedparam(pthread_self(), &current_policy, &current) == 0);
    printf("Darwin priority reset: expected=%d actual=%d policy=%d\n",
           expected.sched_priority, current.sched_priority, current_policy);
    fflush(stdout);
    assert(current_policy == expected_policy);
    assert(current.sched_priority == expected.sched_priority);
    return 0;
}
