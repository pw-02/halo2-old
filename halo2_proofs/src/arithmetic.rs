//! This module provides common utilities, traits and structures for group,
//! field and polynomial arithmetic.
use std::path::Path;
use super::multicore;
pub use ff::Field;
use group::{
    ff::{BatchInvert, PrimeField},
    Curve, Group, GroupOpsOwned, ScalarMulOwned,
};

pub use halo2curves::{CurveAffine, CurveExt};
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use ec_gpu_gen;
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use ec_gpu_gen::rust_gpu_tools::{program_closures, Device, Program, Vendor, CUDA_CORES};
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use ec_gpu_gen::multiexp::MultiexpKernel;
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use halo2curves::bn256::Bn256;
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use ec_gpu_gen::threadpool::Worker;
#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
use ec_gpu_gen::fft::FftKernel;
use std::sync::Arc;
use std::time::Instant;

#[derive(serde::Serialize)]
struct FFTLogInfo {     
    device: String,
    num_gpus: String,
    elements: String,
    degree: String,
    kernel_initialization: String,
    fft_duration: String,
    total_duration: String,
}

struct MSMLogInfo {     
    device: String,
    num_gpus: String,
    elements: String,
    kernel_initialization: String,
    msm_duration: String,
    total_duration: String,
}

/// This represents an element of a group with basic operations that can be
/// performed. This allows an FFT implementation (for example) to operate
/// generically over either a field or elliptic curve group.
pub trait FftGroup<Scalar: Field>:
    Copy + Send + Sync + 'static + GroupOpsOwned + ScalarMulOwned<Scalar>
{
}

impl<T, Scalar> FftGroup<Scalar> for T
where
    Scalar: Field,
    T: Copy + Send + Sync + 'static + GroupOpsOwned + ScalarMulOwned<Scalar>,
{
}

fn multiexp_serial<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C], acc: &mut C::Curve) {
    let coeffs: Vec<_> = coeffs.iter().map(|a| a.to_repr()).collect();

    let c = if bases.len() < 4 {
        1
    } else if bases.len() < 32 {
        3
    } else {
        (f64::from(bases.len() as u32)).ln().ceil() as usize
    };

    fn get_at<F: PrimeField>(segment: usize, c: usize, bytes: &F::Repr) -> usize {
        let skip_bits = segment * c;
        let skip_bytes = skip_bits / 8;

        if skip_bytes >= 32 {
            return 0;
        }

        let mut v = [0; 8];
        for (v, o) in v.iter_mut().zip(bytes.as_ref()[skip_bytes..].iter()) {
            *v = *o;
        }

        let mut tmp = u64::from_le_bytes(v);
        tmp >>= skip_bits - (skip_bytes * 8);
        tmp = tmp % (1 << c);

        tmp as usize
    }

    let segments = (256 / c) + 1;

    for current_segment in (0..segments).rev() {
        for _ in 0..c {
            *acc = acc.double();
        }

        #[derive(Clone, Copy)]
        enum Bucket<C: CurveAffine> {
            None,
            Affine(C),
            Projective(C::Curve),
        }

        impl<C: CurveAffine> Bucket<C> {
            fn add_assign(&mut self, other: &C) {
                *self = match *self {
                    Bucket::None => Bucket::Affine(*other),
                    Bucket::Affine(a) => Bucket::Projective(a + *other),
                    Bucket::Projective(mut a) => {
                        a += *other;
                        Bucket::Projective(a)
                    }
                }
            }

            fn add(self, mut other: C::Curve) -> C::Curve {
                match self {
                    Bucket::None => other,
                    Bucket::Affine(a) => {
                        other += a;
                        other
                    }
                    Bucket::Projective(a) => other + &a,
                }
            }
        }

        let mut buckets: Vec<Bucket<C>> = vec![Bucket::None; (1 << c) - 1];

        for (coeff, base) in coeffs.iter().zip(bases.iter()) {
            let coeff = get_at::<C::Scalar>(current_segment, c, coeff);
            if coeff != 0 {
                buckets[coeff - 1].add_assign(base);
            }
        }

        // Summation by parts
        // e.g. 3a + 2b + 1c = a +
        //                    (a) + b +
        //                    ((a) + b) + c
        let mut running_sum = C::Curve::identity();
        for exp in buckets.into_iter().rev() {
            running_sum = exp.add(running_sum);
            *acc = *acc + &running_sum;
        }
    }
}

/// Performs a small multi-exponentiation operation.
/// Uses the double-and-add algorithm with doublings shared across points.
pub fn small_multiexp<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {
    let coeffs: Vec<_> = coeffs.iter().map(|a| a.to_repr()).collect();
    let mut acc = C::Curve::identity();

    // for byte idx
    for byte_idx in (0..32).rev() {
        // for bit idx
        for bit_idx in (0..8).rev() {
            acc = acc.double();
            // for each coeff
            for coeff_idx in 0..coeffs.len() {
                let byte = coeffs[coeff_idx].as_ref()[byte_idx];
                if ((byte >> bit_idx) & 1) != 0 {
                    acc += bases[coeff_idx];
                }
            }
        }
    }

    acc
}

/// Performs a multi-exponentiation operation.
///
/// This function will panic if coeffs and bases have a different length.
///
/// This will use multithreading if beneficial.
pub fn multiexp_cpu<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {

    let global_timer = Instant::now();

    assert_eq!(coeffs.len(), bases.len());
    let num_threads = multicore::current_num_threads();

    if coeffs.len() > num_threads {
        let timer = Instant::now();

        let chunk = coeffs.len() / num_threads;
        let num_chunks = coeffs.chunks(chunk).len();
        let mut results = vec![C::Curve::identity(); num_chunks];
        multicore::scope(|scope| {
            let chunk = coeffs.len() / num_threads;

            for ((coeffs, bases), acc) in coeffs
                .chunks(chunk)
                .zip(bases.chunks(chunk))
                .zip(results.iter_mut())
            {
                scope.spawn(move |_| {
                    multiexp_serial(coeffs, bases, acc);
                });
            }
        });
       let result = results.iter().fold(C::Curve::identity(), |a, b| a + b);

       let msm_duration = timer.elapsed();
       let total_duration: std::time::Duration = global_timer.elapsed();
       let msm_info = MSMLogInfo{
        device:String::from("cpu"),
        num_gpus:format!("{}",0 as u32),
        elements:format!("{}",bases.len() as u32),
        kernel_initialization: format!("{:?}",0),
        msm_duration: format!("{:?}",msm_duration.as_millis()),
        total_duration: format!("{:?}",total_duration.as_millis()),};
       #[cfg(feature = "logging")]
       log_msm_stats(msm_info);
       result
    } else {

        let timer = Instant::now();
        let mut acc = C::Curve::identity();
        multiexp_serial(coeffs, bases, &mut acc);
        let msm_duration = timer.elapsed();
        let total_duration: std::time::Duration = global_timer.elapsed();
        let msm_info = MSMLogInfo{
         device:String::from("cpu"),
         num_gpus:format!("{}",0 as u32),
         elements:format!("{}",bases.len() as u32),
         kernel_initialization: format!("{:?}",0),
         msm_duration: format!("{:?}",msm_duration.as_millis()),
         total_duration: format!("{:?}",total_duration.as_millis()),};
        #[cfg(feature = "logging")]
        log_msm_stats(msm_info);

        acc
    }
}

/// Performs a multi-exponentiation operation and will attempt to use GPU.
///
/// This function will panic if coeffs and bases have a different length.
///
/// This will use multithreading if beneficial.
//#[cfg(any(feature = "cuda", feature = "opencl"))]
pub fn best_multiexp<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {

    #[cfg(any(feature = "cuda", feature = "opencl"))]
    let result: <C as CurveAffine>::CurveExt = multiexp_gpu(coeffs, bases).unwrap();

    #[cfg(not(any(feature = "cuda", feature = "opencl")))]
    let result: <C as CurveAffine>::CurveExt = multiexp_cpu(coeffs, bases);

    result
}

#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
pub fn multiexp_gpu<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> Result<C::Curve, ec_gpu_gen::EcError>{

    let global_timer = Instant::now();
    let mut timer = Instant::now();

    //uses the ec-gpu crate
    let devices = Device::all();
    let mut kern = MultiexpKernel::<Bn256>::create(&devices).expect("Cannot initialize kernel!");
    let kernel_initialization = timer.elapsed();

    let pool = Worker::new();
    let t: Arc<Vec<_>> = Arc::new(coeffs.iter().map(|a| a.to_repr()).collect());
    let g:Arc<Vec<_>> = Arc::new(bases.to_vec().clone());
    let g2 = (g.clone(), 0);
    let (bss, skip) =  (g2.0.clone(), g2.1);
    
    timer = Instant::now();
    let gpu_result = kern.multiexp(&pool, bss, t, skip).map_err(Into::into);
    let msm_duration = timer.elapsed();

    let total_duration: std::time::Duration = global_timer.elapsed();

    let msm_info = MSMLogInfo{
        device:String::from("gpu"),
        num_gpus:format!("{}",devices.len() as u32),
        elements:format!("{}",coeffs.len() as u32),
        kernel_initialization: format!("{:?}",kernel_initialization.as_millis()),
        msm_duration: format!("{:?}",msm_duration.as_millis()),
        total_duration: format!("{:?}",total_duration.as_millis()),
    };
    #[cfg(feature = "logging")]
    log_msm_stats(msm_info);

    gpu_result
}





/// Performs a radix-$2$ Fast-Fourier Transformation (FFT) on a vector of size
/// $n = 2^k$, when provided `log_n` = $k$ and an element of multiplicative
/// order $n$ called `omega` ($\omega$). The result is that the vector `a`, when
/// interpreted as the coefficients of a polynomial of degree $n - 1$, is
/// transformed into the evaluations of this polynomial at each of the $n$
/// distinct powers of $\omega$. This transformation is invertible by providing
/// $\omega^{-1}$ in place of $\omega$ and dividing each resulting field element
/// by $n$.
///
/// This will use multithreading if beneficial.
pub fn cpu_fft<Scalar: Field, G: FftGroup<Scalar>>(a: &mut [G], omega: Scalar, log_n: u32) {

    let global_timer = Instant::now();

    fn bitreverse(mut n: usize, l: usize) -> usize {
        let mut r = 0;
        for _ in 0..l {
            r = (r << 1) | (n & 1);
            n >>= 1;
        }
        r
    }

    let threads = multicore::current_num_threads();
    let log_threads = log2_floor(threads);
    let n = a.len() as usize;
    assert_eq!(n, 1 << log_n);

    for k in 0..n {
        let rk = bitreverse(k, log_n as usize);
        if k < rk {
            a.swap(rk, k);
        }
    }

    // precompute twiddle factors
    let twiddles: Vec<_> = (0..(n / 2) as usize)
        .scan(Scalar::ONE, |w, _| {
            let tw = *w;
            *w *= &omega;
            Some(tw)
        })
        .collect();


    let timer = Instant::now();

    if log_n <= log_threads {
        let mut chunk = 2_usize;
        let mut twiddle_chunk = (n / 2) as usize;
        for _ in 0..log_n {
            a.chunks_mut(chunk).for_each(|coeffs| {
                let (left, right) = coeffs.split_at_mut(chunk / 2);

                // case when twiddle factor is one
                let (a, left) = left.split_at_mut(1);
                let (b, right) = right.split_at_mut(1);
                let t = b[0];
                b[0] = a[0];
                a[0] += &t;
                b[0] -= &t;

                left.iter_mut()
                    .zip(right.iter_mut())
                    .enumerate()
                    .for_each(|(i, (a, b))| {
                        let mut t = *b;
                        t *= &twiddles[(i + 1) * twiddle_chunk];
                        *b = *a;
                        *a += &t;
                        *b -= &t;
                    });
            });
            chunk *= 2;
            twiddle_chunk /= 2;
        }
    } else {
        recursive_butterfly_arithmetic(a, n, 1, &twiddles)
    }
    let fft_duration: std::time::Duration = timer.elapsed();

    let total_duration: std::time::Duration = global_timer.elapsed();

    let fft_info = FFTLogInfo{
        device:String::from("cpu"),
        num_gpus:format!("{}",0 as u32),
        elements:format!("{}",1u64 << log_n),
        degree:format!("{}",log_n as u32),
        kernel_initialization: format!("{:?}", 0 as u32),
        fft_duration: format!("{:?}",fft_duration.as_millis()),
        total_duration: format!("{:?}",total_duration.as_millis()),

    };
    #[cfg(feature = "logging")]
    log_fft_stats(fft_info);
}

pub fn best_fft<Scalar: Field, G: FftGroup<Scalar>>(a: &mut [G], omega: Scalar, log_n: u32) {
    #[cfg(any(feature = "cuda", feature = "opencl"))]
    gpu_fft(a, omega, log_n);

    #[cfg(not(any(feature = "cuda", feature = "opencl")))]
    cpu_fft(a, omega, log_n);

}

#[cfg(any(feature = "cuda", feature = "opencl",feature = "logging"))]
pub fn gpu_fft<Scalar: Field, G: FftGroup<Scalar>>(a: &mut [G], omega: Scalar, log_n: u32) {
    
    let global_timer = Instant::now();
    let mut timer = Instant::now();
    let devices = Device::all();
    let mut kern = FftKernel::<Bn256>::create(&devices).expect("Cannot initialize kernel!");
    let kernel_initialization = timer.elapsed();
    timer = Instant::now();
    kern.radix_fft_many(&mut [a], &[omega], &[log_n]).expect("GPU FFT failed!");
    let fft_duration = timer.elapsed();

    let total_duration: std::time::Duration = global_timer.elapsed();

    let fft_info = FFTLogInfo{
        device:String::from("gpu"),
        num_gpus:format!("{}",devices.len() as u32),
        elements:format!("{}",1u64 << log_n),
        degree:format!("{}",log_n as u32),
        kernel_initialization: format!("{:?}",kernel_initialization.as_millis()),
        fft_duration: format!("{:?}",fft_duration.as_millis()),
        total_duration: format!("{:?}",total_duration.as_millis()),

    };
    #[cfg(feature = "logging")]
    log_fft_stats(fft_info);


}


/*pub fn fft_gpu<Scalar: Field, G: FftGroup<Scalar>>(a: &mut [G], omega: Scalar, log_n: u32) {
    //uses the ec-gpu crate
    let devices = Device::all();
    let mut kern = FftKernel::<Bn256>::create(&devices).expect("Cannot initialize kernel!");
    kern.radix_fft_many(&mut [a], &[omega], &[log_n]).expect("GPU FFT failed!");
}
*/

/// This perform recursive butterfly arithmetic
pub fn recursive_butterfly_arithmetic<Scalar: Field, G: FftGroup<Scalar>>(
    a: &mut [G],
    n: usize,
    twiddle_chunk: usize,
    twiddles: &[Scalar],
) {
    if n == 2 {
        let t = a[1];
        a[1] = a[0];
        a[0] += &t;
        a[1] -= &t;
    } else {
        let (left, right) = a.split_at_mut(n / 2);
        rayon::join(
            || recursive_butterfly_arithmetic(left, n / 2, twiddle_chunk * 2, twiddles),
            || recursive_butterfly_arithmetic(right, n / 2, twiddle_chunk * 2, twiddles),
        );

        // case when twiddle factor is one
        let (a, left) = left.split_at_mut(1);
        let (b, right) = right.split_at_mut(1);
        let t = b[0];
        b[0] = a[0];
        a[0] += &t;
        b[0] -= &t;

        left.iter_mut()
            .zip(right.iter_mut())
            .enumerate()
            .for_each(|(i, (a, b))| {
                let mut t = *b;
                t *= &twiddles[(i + 1) * twiddle_chunk];
                *b = *a;
                *a += &t;
                *b -= &t;
            });
    }
}

/// Convert coefficient bases group elements to lagrange basis by inverse FFT.
pub fn g_to_lagrange<C: CurveAffine>(g_projective: Vec<C::Curve>, k: u32) -> Vec<C> {
    let n_inv = C::Scalar::TWO_INV.pow_vartime(&[k as u64, 0, 0, 0]);
    let mut omega_inv = C::Scalar::ROOT_OF_UNITY_INV;
    for _ in k..C::Scalar::S {
        omega_inv = omega_inv.square();
    }

    let mut g_lagrange_projective = g_projective;
    best_fft(&mut g_lagrange_projective, omega_inv, k);
    parallelize(&mut g_lagrange_projective, |g, _| {
        for g in g.iter_mut() {
            *g *= n_inv;
        }
    });

    let mut g_lagrange = vec![C::identity(); 1 << k];
    parallelize(&mut g_lagrange, |g_lagrange, starts| {
        C::Curve::batch_normalize(
            &g_lagrange_projective[starts..(starts + g_lagrange.len())],
            g_lagrange,
        );
    });

    g_lagrange
}

/// This evaluates a provided polynomial (in coefficient form) at `point`.
pub fn eval_polynomial<F: Field>(poly: &[F], point: F) -> F {
    fn evaluate<F: Field>(poly: &[F], point: F) -> F {
        poly.iter()
            .rev()
            .fold(F::ZERO, |acc, coeff| acc * point + coeff)
    }
    let n = poly.len();
    let num_threads = multicore::current_num_threads();
    if n * 2 < num_threads {
        evaluate(poly, point)
    } else {
        let chunk_size = (n + num_threads - 1) / num_threads;
        let mut parts = vec![F::ZERO; num_threads];
        multicore::scope(|scope| {
            for (chunk_idx, (out, poly)) in
                parts.chunks_mut(1).zip(poly.chunks(chunk_size)).enumerate()
            {
                scope.spawn(move |_| {
                    let start = chunk_idx * chunk_size;
                    out[0] = evaluate(poly, point) * point.pow_vartime(&[start as u64, 0, 0, 0]);
                });
            }
        });
        parts.iter().fold(F::ZERO, |acc, coeff| acc + coeff)
    }
}

/// This computes the inner product of two vectors `a` and `b`.
///
/// This function will panic if the two vectors are not the same size.
pub fn compute_inner_product<F: Field>(a: &[F], b: &[F]) -> F {
    // TODO: parallelize?
    assert_eq!(a.len(), b.len());

    let mut acc = F::ZERO;
    for (a, b) in a.iter().zip(b.iter()) {
        acc += (*a) * (*b);
    }

    acc
}

/// Divides polynomial `a` in `X` by `X - b` with
/// no remainder.
pub fn kate_division<'a, F: Field, I: IntoIterator<Item = &'a F>>(a: I, mut b: F) -> Vec<F>
where
    I::IntoIter: DoubleEndedIterator + ExactSizeIterator,
{
    b = -b;
    let a = a.into_iter();

    let mut q = vec![F::ZERO; a.len() - 1];

    let mut tmp = F::ZERO;
    for (q, r) in q.iter_mut().rev().zip(a.rev()) {
        let mut lead_coeff = *r;
        lead_coeff.sub_assign(&tmp);
        *q = lead_coeff;
        tmp = lead_coeff;
        tmp.mul_assign(&b);
    }

    q
}

/// This utility function will parallelize an operation that is to be
/// performed over a mutable slice.
pub fn parallelize<T: Send, F: Fn(&mut [T], usize) + Send + Sync + Clone>(v: &mut [T], f: F) {
    // Algorithm rationale:
    //
    // Using the stdlib `chunks_mut` will lead to severe load imbalance.
    // From https://github.com/rust-lang/rust/blob/e94bda3/library/core/src/slice/iter.rs#L1607-L1637
    // if the division is not exact, the last chunk will be the remainder.
    //
    // Dividing 40 items on 12 threads will lead to a chunk size of 40/12 = 3,
    // There will be a 13 chunks of size 3 and 1 of size 1 distributed on 12 threads.
    // This leads to 1 thread working on 6 iterations, 1 on 4 iterations and 10 on 3 iterations,
    // a load imbalance of 2x.
    //
    // Instead we can divide work into chunks of size
    // 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3 = 4*4 + 3*8 = 40
    //
    // This would lead to a 6/4 = 1.5x speedup compared to naive chunks_mut
    //
    // See also OpenMP spec (page 60)
    // http://www.openmp.org/mp-documents/openmp-4.5.pdf
    // "When no chunk_size is specified, the iteration space is divided into chunks
    // that are approximately equal in size, and at most one chunk is distributed to
    // each thread. The size of the chunks is unspecified in this case."
    // This implies chunks are the same size ±1

    let f = &f;
    let total_iters = v.len();
    let num_threads = multicore::current_num_threads();
    let base_chunk_size = total_iters / num_threads;
    let cutoff_chunk_id = total_iters % num_threads;
    let split_pos = cutoff_chunk_id * (base_chunk_size + 1);
    let (v_hi, v_lo) = v.split_at_mut(split_pos);

    multicore::scope(|scope| {
        // Skip special-case: number of iterations is cleanly divided by number of threads.
        if cutoff_chunk_id != 0 {
            for (chunk_id, chunk) in v_hi.chunks_exact_mut(base_chunk_size + 1).enumerate() {
                let offset = chunk_id * (base_chunk_size + 1);
                scope.spawn(move |_| f(chunk, offset));
            }
        }
        // Skip special-case: less iterations than number of threads.
        if base_chunk_size != 0 {
            for (chunk_id, chunk) in v_lo.chunks_exact_mut(base_chunk_size).enumerate() {
                let offset = split_pos + (chunk_id * base_chunk_size);
                scope.spawn(move |_| f(chunk, offset));
            }
        }
    });
}

fn log2_floor(num: usize) -> u32 {
    assert!(num > 0);

    let mut pow = 0;

    while (1 << (pow + 1)) <= num {
        pow += 1;
    }

    pow
}

/// Returns coefficients of an n - 1 degree polynomial given a set of n points
/// and their evaluations. This function will panic if two values in `points`
/// are the same.
pub fn lagrange_interpolate<F: Field>(points: &[F], evals: &[F]) -> Vec<F> {
    assert_eq!(points.len(), evals.len());
    if points.len() == 1 {
        // Constant polynomial
        vec![evals[0]]
    } else {
        let mut denoms = Vec::with_capacity(points.len());
        for (j, x_j) in points.iter().enumerate() {
            let mut denom = Vec::with_capacity(points.len() - 1);
            for x_k in points
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != j)
                .map(|a| a.1)
            {
                denom.push(*x_j - x_k);
            }
            denoms.push(denom);
        }
        // Compute (x_j - x_k)^(-1) for each j != i
        denoms.iter_mut().flat_map(|v| v.iter_mut()).batch_invert();

        let mut final_poly = vec![F::ZERO; points.len()];
        for (j, (denoms, eval)) in denoms.into_iter().zip(evals.iter()).enumerate() {
            let mut tmp: Vec<F> = Vec::with_capacity(points.len());
            let mut product = Vec::with_capacity(points.len() - 1);
            tmp.push(F::ONE);
            for (x_k, denom) in points
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != j)
                .map(|a| a.1)
                .zip(denoms.into_iter())
            {
                product.resize(tmp.len() + 1, F::ZERO);
                for ((a, b), product) in tmp
                    .iter()
                    .chain(std::iter::once(&F::ZERO))
                    .zip(std::iter::once(&F::ZERO).chain(tmp.iter()))
                    .zip(product.iter_mut())
                {
                    *product = *a * (-denom * x_k) + *b * denom;
                }
                std::mem::swap(&mut tmp, &mut product);
            }
            assert_eq!(tmp.len(), points.len());
            assert_eq!(product.len(), points.len() - 1);
            for (final_coeff, interpolation_coeff) in final_poly.iter_mut().zip(tmp.into_iter()) {
                *final_coeff += interpolation_coeff * eval;
            }
        }
        final_poly
    }
}

pub(crate) fn evaluate_vanishing_polynomial<F: Field>(roots: &[F], z: F) -> F {
    fn evaluate<F: Field>(roots: &[F], z: F) -> F {
        roots.iter().fold(F::ONE, |acc, point| (z - point) * acc)
    }
    let n = roots.len();
    let num_threads = multicore::current_num_threads();
    if n * 2 < num_threads {
        evaluate(roots, z)
    } else {
        let chunk_size = (n + num_threads - 1) / num_threads;
        let mut parts = vec![F::ONE; num_threads];
        multicore::scope(|scope| {
            for (out, roots) in parts.chunks_mut(1).zip(roots.chunks(chunk_size)) {
                scope.spawn(move |_| out[0] = evaluate(roots, z));
            }
        });
        parts.iter().fold(F::ONE, |acc, part| acc * part)
    }
}

pub(crate) fn powers<F: Field>(base: F) -> impl Iterator<Item = F> {
    std::iter::successors(Some(F::ONE), move |power| Some(base * power))
}

fn log_msm_stats(msm_info: MSMLogInfo)
{   
    let log_path = "logs";
    let log_name = "msm.csv";
    //let log_file = format!("{}/{}", log_path, log_name);
    let log_file = format!("C:\\Users\\pw\\projects\\dist-zkml\\halo2\\logs\\msm.csv");

    //let params_path = Path::new(&log_file);

    let already_exists= Path::new(&log_file).exists();

    let file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .append(true)
    .open(log_file)
    .unwrap();

    let mut wtr = csv::Writer::from_writer(file);

    if already_exists == false
    {
        let _ = wtr.write_record(&["device","num_gpus", "elements", "kernel_initialization(ms)", 
        "msm_duration(ms)", "total_duration(ms)",]);    
    }
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let _ = wtr.write_record(&[msm_info.device, msm_info.num_gpus, msm_info.elements,
        msm_info.kernel_initialization, msm_info.msm_duration, msm_info.total_duration,]);
    let _ = wtr.flush();
}


fn log_fft_stats(fft_info: FFTLogInfo)
{   

    let log_path = "logs";
    let log_name = "fft.csv";
    //let log_file = format!("{}/{}", log_path, log_name);
    let log_file = format!("C:\\Users\\pw\\projects\\dist-zkml\\halo2\\logs\\fft.csv");

    //let params_path = Path::new(&log_file);

    let already_exists= Path::new(&log_file).exists();

    let file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .append(true)
    .open(log_file)
    .unwrap();

    let mut wtr = csv::Writer::from_writer(file);

    if already_exists == false
    {
        let _ = wtr.write_record(&["device","num_gpus", "elements","degree", "kernel_initialization(ms)", 
        "fft_duration(ms)", "total_duration(ms)",]);    
    }
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let _ = wtr.write_record(&[fft_info.device, fft_info.num_gpus, fft_info.elements, fft_info.degree,
        fft_info.kernel_initialization, fft_info.fft_duration, fft_info.total_duration,]);
    let _ = wtr.flush();
}


#[cfg(test)]
use rand_core::OsRng;

#[cfg(test)]
use crate::halo2curves::pasta::Fp;

#[test]
fn test_lagrange_interpolate() {
    let rng = OsRng;

    let points = (0..5).map(|_| Fp::random(rng)).collect::<Vec<_>>();
    let evals = (0..5).map(|_| Fp::random(rng)).collect::<Vec<_>>();

    for coeffs in 0..5 {
        let points = &points[0..coeffs];
        let evals = &evals[0..coeffs];

        let poly = lagrange_interpolate(points, evals);
        assert_eq!(poly.len(), points.len());

        for (point, eval) in points.iter().zip(evals) {
            assert_eq!(eval_polynomial(&poly, *point), *eval);
        }
    }
}


#[test]
fn test_gpu_msm() 
{
    use rand_core::OsRng;
    use halo2curves::pairing::Engine;

    let k = 2;
    let max_size = 1 << (k + 1);
    //let mut rng = Pcg32::seed_from_u64(42);
    let rng = OsRng;
    let multiexp_scalars_2:Vec<<Bn256 as Engine>::Scalar> = (0..max_size)
        .map(|_| <Bn256 as Engine>::Scalar::random(rng))
        .collect();
      
    let multiexp_bases_2: Vec<<Bn256 as Engine>::G1Affine> = (0..max_size)
    .map(|_| <Bn256 as Engine>::G1::random(rng).to_affine())
    .collect();
    
    let gpu = multiexp_gpu(&multiexp_scalars_2, &multiexp_bases_2).unwrap(); 
    println!("gpu:{:?}",  gpu.to_affine());
}

#[test]
fn test_best_msm() {
    use halo2curves::pairing::Engine;
    use halo2curves::bn256::Fr;
    use rand_pcg::Pcg32;
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use multiexp_gpu;
    use multiexp_cpu;

    const MIN_MSM_SIZE: usize = 10;
    const MAX_MSM_SIZE: usize = 15;

    let mut rng: rand_pcg::Lcg64Xsh32 = Pcg32::seed_from_u64(42);

    for k in MIN_MSM_SIZE..=MAX_MSM_SIZE {

        let samples = 1 << (k);

        println!("Testing Multiexp for {} elements...", samples);

        let coeffs:Vec<<Bn256 as Engine>::Scalar> = (0..samples)
        .map(|_| <Bn256 as Engine>::Scalar::random(&mut rng))
        .collect();

        let bases: Vec<<Bn256 as Engine>::G1Affine> = (0..samples)
        .map(|_| <Bn256 as Engine>::G1::random(&mut rng).to_affine())
        .collect();
    
        let mut now = std::time::Instant::now();
        let cpu = multiexp_cpu(&coeffs, &bases);
        let cpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("CPU took {}ms.", cpu_dur);

        now = std::time::Instant::now();
        let gpu = multiexp_gpu(&coeffs, &bases).unwrap();
        let gpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("GPU took {}ms.", gpu_dur);

        println!("Speedup: x{}", cpu_dur as f32 / gpu_dur as f32);

        println!("cpu:{:?}",  cpu.to_affine());
        println!("gpu:{:?}",  gpu.to_affine());
        assert_eq!(cpu.to_affine(), gpu.to_affine())

    }
}




#[test]
fn test_gpu_fft() {
    use crate::poly::EvaluationDomain;
    use halo2curves::bn256::Fr;
    use rand_core::OsRng;

    for k in 8..=16 {
        let rng = OsRng;
        // polynomial degree n = 2^k
        let n = 1u64 << k;
        // polynomial coeffs
        let mut coeffs: Vec<_> = (0..n).map(|_| Fr::random(rng)).collect();
        // evaluation domain
        let domain: EvaluationDomain<Fr> = EvaluationDomain::new(1, k);

        println!("Testing FFT for {} elements, degree {}...", n, k);

        let now = std::time::Instant::now();

        best_fft(&mut coeffs, domain.get_omega(), k);
        let gpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("GPU took {}ms.", gpu_dur);

    }
}


#[test]
fn test_best_fft() {
    use crate::poly::EvaluationDomain;
    use halo2curves::bn256::Fr;
    use rand_pcg::Pcg32;
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use cpu_fft;
    use gpu_fft;

    const MIN_FFT_SIZE: u32 = 20;
    const MAX_FFT_SIZE: u32 = 24;
    
    for k in MIN_FFT_SIZE..=MAX_FFT_SIZE {
        let mut rng = Pcg32::seed_from_u64(42);
        // poly: OsRngnomial degree n = 2^k
        let n = 1u64 << k;
        // polynomial coeffs
        let coeffs: Vec<_> = (0..n).map(|_| Fr::random(&mut rng)).collect();
        // evaluation domain
        let domain: EvaluationDomain<Fr> = EvaluationDomain::new(1, k);

        println!("Testing FFT for {} elements, degree {}...", n, k);

        let mut prev_fft_coeffs = coeffs.clone();

        let mut now = std::time::Instant::now();
        
        cpu_fft(&mut prev_fft_coeffs, domain.get_omega(), k);
        let cpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("CPU took {}ms.", cpu_dur);

        let mut optimized_fft_coeffs = coeffs.clone();
        now = std::time::Instant::now();
        
        gpu_fft(&mut optimized_fft_coeffs, domain.get_omega(), k);

        let gpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("GPU took {}ms.", gpu_dur);

        println!("Speedup: x{}", cpu_dur as f32 / gpu_dur as f32);
        assert_eq!(prev_fft_coeffs, optimized_fft_coeffs);
    }
}



#[test]
fn test_best_fft_multiple_gpu() {
    use crate::poly::EvaluationDomain;
    use halo2curves::bn256::Fr;
    use rand_core::OsRng;

    for k in 21..=23 {
        let rng = OsRng;
        // polynomial degree n = 2^k
        let n = 1u64 << k;
        // polynomial coeffs
        let coeffs: Vec<_> = (0..n).map(|_| Fr::random(rng)).collect();
        // evaluation domain
        let domain: EvaluationDomain<Fr> = EvaluationDomain::new(1, k);

        println!("Testing FFT for {} elements, degree {}...", n, k);

        let mut prev_fft_coeffs = coeffs.clone();

        let mut now = std::time::Instant::now();
        
        best_fft(&mut prev_fft_coeffs, domain.get_omega(), k);
        let cpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("CPU took {}ms.", cpu_dur);

        let mut optimized_fft_coeffs = coeffs.clone();
        now = std::time::Instant::now();
        
        best_fft(&mut optimized_fft_coeffs, domain.get_omega(), k);

        let gpu_dur = now.elapsed().as_secs() * 1000 + now.elapsed().subsec_millis() as u64;
        println!("GPU took {}ms.", gpu_dur);
        assert_eq!(prev_fft_coeffs, optimized_fft_coeffs);
    }
}



