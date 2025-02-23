# Unreleased Changes in The Mainline

## Breaking Changes

## Other Changes and Enhancements

* DKIM signer TTLs can be now be expressed using duration strings like `"5
  mins"`. Previously you could only use the integer number of seconds.
* debian packages will now unmask kumod and tsa-daemon services as part
  of post installation.  Thanks to @cai-n! #331
* [memoize](../reference/kumo/memoize.md) now has an optional
  `invalidate_with_epoch` parameter that allows you to opt a specific cache
  into epoch-based invalidation.
* DKIM signer has a separate supplemental cache for the parsed key data,
  which helps to reduce latency for deployments where the same key data
  is shared between multiple signing domains.
* New [msg:shrink()](../reference/message/shrink.md) and
  [msg:shrink_data()](../reference/message/shrink_data.md) methods.
* Added various python compatibility functions to the minijinja template engine.
  See [the pycompat
  docs](https://docs.rs/minijinja-contrib/latest/minijinja_contrib/pycompat/fn.unknown_method_callback.html)
  for a list of the additional functions.
* New [kumo.string.eval_template](../reference/string/eval_template.md)
  function for expanding minijinja template strings.
* New [low_memory_reduction_policy](../reference/kumo/make_egress_path/low_memory_reduction_policy.md),
  [no_memory_reduction_policy](../reference/kumo/make_egress_path/no_memory_reduction_policy.md) and
  options give advanced control over memory vs. spool IO trade-offs when
  available is memory low.
* New [shrink_policy](../reference/kumo/make_queue_config/shrink_policy.md)
  option to give advanced control over memory vs. spool IO trade-offs when
  messages are delayed.

## Fixes

* When using
  [kumo.dkim.set_signing_threads](../reference/kumo.dkim/set_signing_threads.md),
  some extraneous unused threads would be created.
* Using a display name with commas in the builder mode of the HTTP injection
  API would produce an invalid mailbox header.
